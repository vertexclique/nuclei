//! A strictly first-come, first-served counting semaphore for one shard.
//!
//! The permits live in a `Cell` and the waiters in the shared
//! [`Waiters`](super::waiters) list, so no operation touches an atomic. A
//! request is served in arrival order and nobody barges: permits that come
//! free are kept for the front waiter until its whole request fits, and
//! neither a later waiter nor [`Semaphore::try_acquire`] takes them meanwhile.
//! Dropping a waiting [`Acquire`] gives up its place; dropping one that was
//! already granted passes its permits on.

use core::cell::Cell;
use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::waiters::{Key, State, Waiters, wake_closed};
use crate::sync::{AcquireError, TryAcquireError};

/// A counting semaphore whose waits are futures, for the tasks of one shard.
///
/// A request for `n` permits waits in line behind the requests that arrived
/// before it, and is granted when it reaches the front and `n` permits are
/// free. A large request at the front is therefore never bypassed by smaller
/// ones behind it, and [`try_acquire`](Self::try_acquire) fails while anyone
/// waits. [`close`](Self::close) ends every wait with an [`AcquireError`].
///
/// It is `!Send` and `!Sync`: share it between the tasks of a shard with an
/// `Rc`.
///
/// ```
/// use nuclei::sync::TryAcquireError;
/// use nuclei::sync::local::Semaphore;
///
/// let budget = Semaphore::new(4);
/// let first = budget.try_acquire_many(3).expect("three of four are free");
/// assert_eq!(budget.try_acquire_many(2).unwrap_err(), TryAcquireError::NoPermits);
/// drop(first);
/// assert_eq!(budget.available_permits(), 4);
/// ```
pub struct Semaphore {
    /// Signed: a channel receiver that unwinds takes back an admission it
    /// handed on (`debit`), which can leave the count below zero until the
    /// next release pays it.
    permits: Cell<isize>,
    closed: Cell<bool>,
    waiters: Waiters,
}

impl Semaphore {
    /// The most permits a semaphore can hold: `usize::MAX >> 3`.
    pub const MAX_PERMITS: usize = usize::MAX >> 3;

    /// A semaphore holding `permits` permits.
    ///
    /// # Panics
    ///
    /// When `permits` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS).
    pub const fn new(permits: usize) -> Self {
        assert!(
            permits <= Self::MAX_PERMITS,
            "a semaphore cannot hold more than Semaphore::MAX_PERMITS"
        );
        Self {
            permits: Cell::new(permits as isize),
            closed: Cell::new(false),
            waiters: Waiters::new(),
        }
    }

    /// The permits free right now, including the ones kept for the front
    /// waiter until its request fits.
    pub fn available_permits(&self) -> usize {
        self.permits.get().max(0) as usize
    }

    /// Adds `n` new permits, handing them to waiters first. Permits added to
    /// a closed semaphore are counted, but nothing is handed out.
    ///
    /// # Panics
    ///
    /// When the semaphore would hold more than
    /// [`MAX_PERMITS`](Self::MAX_PERMITS).
    #[track_caller]
    pub fn add_permits(&self, n: usize) {
        let total = self
            .permits
            .get()
            .saturating_add(n.min(Self::MAX_PERMITS + 1) as isize);
        assert!(
            total <= Self::MAX_PERMITS as isize,
            "a semaphore cannot hold more than Semaphore::MAX_PERMITS"
        );
        self.permits.set(total);
        self.serve();
    }

    /// Removes up to `n` free permits for good and returns how many it
    /// removed, which is fewer than `n` when fewer are free. Permits held by
    /// outstanding [`SemaphorePermit`]s are not touched.
    pub fn forget_permits(&self, n: usize) -> usize {
        let removed = n.min(self.available_permits());
        self.permits.set(self.permits.get() - removed as isize);
        removed
    }

    /// Waits for one permit.
    ///
    /// Completes with an [`AcquireError`] once the semaphore is
    /// [closed](Self::close).
    pub fn acquire(&self) -> Acquire<'_> {
        self.acquire_many(1)
    }

    /// Waits for `n` permits, which it takes together.
    ///
    /// Completes with an [`AcquireError`] once the semaphore is
    /// [closed](Self::close).
    ///
    /// # Panics
    ///
    /// When `n` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS), which only a
    /// target with a word narrower than 64 bits allows and which no
    /// semaphore can ever satisfy.
    #[track_caller]
    pub fn acquire_many(&self, n: u32) -> Acquire<'_> {
        Acquire {
            sem: self,
            n: checked_permits(n),
            key: None,
        }
    }

    /// Takes one permit if it is free and nobody waits.
    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        self.try_acquire_many(1)
    }

    /// Takes `n` permits together if they are free and nobody waits.
    ///
    /// # Panics
    ///
    /// When `n` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS), as for
    /// [`acquire_many`](Self::acquire_many).
    #[track_caller]
    pub fn try_acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        let permits = checked_permits(n);
        if self.closed.get() {
            return Err(TryAcquireError::Closed);
        }
        if !self.waiters.is_empty() || self.permits.get() < permits as isize {
            return Err(TryAcquireError::NoPermits);
        }
        self.permits.set(self.permits.get() - permits as isize);
        Ok(SemaphorePermit { sem: self, permits })
    }

    /// Closes the semaphore. Every wait still queued ends in an
    /// [`AcquireError`], and every later acquire fails at once:
    /// `try_acquire` and its sibling return [`TryAcquireError::Closed`],
    /// whatever permits are free. A wait that was granted its permits before
    /// the close keeps them. Permits returned or added afterwards are
    /// counted, and [`available_permits`](Self::available_permits) shows
    /// them, but none is ever handed out. Closing twice does nothing.
    pub fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        wake_closed(&self.waiters);
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// Takes `permits` back and serves the line.
    pub(crate) fn release(&self, permits: usize) {
        self.permits
            .set(self.permits.get().saturating_add(permits as isize));
        self.serve();
    }

    /// Takes back `n` permits that were handed on, as a debt: the count goes
    /// below zero if need be, and the releases that follow pay it.
    pub(crate) fn debit(&self, n: usize) {
        self.permits.set(self.permits.get() - n as isize);
    }

    /// Whether exactly `n` permits are free and none is owed.
    #[inline]
    pub(crate) fn holds(&self, n: usize) -> bool {
        self.permits.get() == n as isize
    }

    /// Grants the front waiters while their requests fit: the look is
    /// inline, the loop out of line since most releases meet no waiter.
    #[inline]
    fn serve(&self) {
        if self.closed.get() {
            return;
        }
        if let Ok(available) = usize::try_from(self.permits.get())
            && self.waiters.front_fits(available)
        {
            self.serve_waiters();
        }
    }

    /// Grants the front waiters while their requests fit. Each waker is woken
    /// outside any borrow, and the state is read again after the wake. A
    /// waker that panics does not leave the next waiters unserved: the guard
    /// serves them and the panic goes on.
    #[inline(never)]
    fn serve_waiters(&self) {
        let rest = ServeRest(self);
        while let Ok(available) = usize::try_from(self.permits.get())
            && let Some((need, waker)) = self.waiters.grant_front_if(available)
        {
            self.permits.set(self.permits.get() - need as isize);
            if let Some(waker) = waker {
                waker.wake();
            }
        }
        core::mem::forget(rest);
    }
}

/// While alive, an unwind serves the waiters the panicking wake left.
struct ServeRest<'a>(&'a Semaphore);

impl Drop for ServeRest<'_> {
    fn drop(&mut self) {
        crate::sync::internal::during_unwind(|| self.0.serve());
    }
}

/// `n` as a permit count, or a panic when no semaphore could ever satisfy it.
#[expect(
    clippy::panic,
    reason = "documented under `# Panics` of the calls that take permits"
)]
#[track_caller]
fn checked_permits(n: u32) -> usize {
    match usize::try_from(n) {
        Ok(permits) if permits <= Semaphore::MAX_PERMITS => permits,
        _ => panic!("a request exceeds Semaphore::MAX_PERMITS"),
    }
}

impl fmt::Debug for Semaphore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Semaphore")
            .field("available_permits", &self.available_permits())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// Permits taken from a [`Semaphore`], returned when dropped.
#[must_use = "dropping a permit returns it at once"]
pub struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
    permits: usize,
}

impl<'a> SemaphorePermit<'a> {
    pub(crate) fn new(sem: &'a Semaphore, permits: usize) -> Self {
        Self { sem, permits }
    }

    /// Keeps the permits out of the semaphore for good.
    pub fn forget(mut self) {
        self.permits = 0;
    }

    /// How many permits this holds.
    pub fn num_permits(&self) -> usize {
        self.permits
    }

    /// Merges `other` into this permit, so one drop returns both.
    ///
    /// # Panics
    ///
    /// When the permits come from different semaphores.
    #[track_caller]
    pub fn merge(&mut self, mut other: Self) {
        assert!(
            core::ptr::eq(self.sem, other.sem),
            "merging permits from different semaphore instances"
        );
        // Both are of one semaphore, whose permits never pass `MAX_PERMITS`.
        self.permits += other.permits;
        other.permits = 0;
    }
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        if self.permits > 0 {
            self.sem.release(self.permits);
        }
    }
}

impl fmt::Debug for SemaphorePermit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemaphorePermit")
            .field("permits", &self.permits)
            .finish()
    }
}

/// The wait for permits of [`Semaphore::acquire`] and
/// [`Semaphore::acquire_many`].
///
/// Cancel safe: dropping it gives up its place in line; if its permits were
/// already granted, they go back and the next waiter is served.
#[must_use = "futures do nothing unless polled"]
pub struct Acquire<'a> {
    sem: &'a Semaphore,
    n: usize,
    key: Option<Key>,
}

impl Acquire<'_> {
    /// Gives up the place in line, or passes on the permits granted; idempotent.
    /// One that never queued (the common acquire, served at once) has nothing
    /// to give up: one inline look.
    #[inline]
    pub(crate) fn cancel(&mut self) {
        if self.key.is_some() {
            self.leave_line();
        }
    }

    /// [`cancel`](Self::cancel) of an acquire in line, out of line.
    #[inline(never)]
    fn leave_line(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let (state, was_front, waker) = self.sem.waiters.cancel(key);
        drop(waker);
        match state {
            State::Waiting if was_front => self.sem.serve(),
            State::Granted => self.sem.release(self.n),
            _ => {}
        }
    }
}

impl<'a> Acquire<'a> {
    /// Joins the line, out of line since most acquires are served at once.
    #[cold]
    #[inline(never)]
    fn queue(&mut self, cx: &mut Context<'_>) -> Poll<<Self as Future>::Output> {
        let this = self;
        let sem = this.sem;
        // The clone is user code that may release permits or close: look
        // again after it.
        let waker = cx.waker().clone();
        if sem.closed.get() {
            crate::coop::spent_one();
            drop(waker);
            return Poll::Ready(Err(AcquireError::closed()));
        }
        if sem.waiters.is_empty() && sem.permits.get() >= this.n as isize {
            sem.permits.set(sem.permits.get() - this.n as isize);
            crate::coop::spent_one();
            // Built before the drop: a panicking drop still returns them.
            let permit = SemaphorePermit::new(sem, this.n);
            drop(waker);
            return Poll::Ready(Ok(permit));
        }
        this.key = Some(sem.waiters.push(this.n, waker));
        Poll::Pending
    }
}

impl<'a> Future for Acquire<'a> {
    type Output = Result<SemaphorePermit<'a>, AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let sem = this.sem;
        if let Some(key) = this.key {
            match sem.waiters.refresh(key, cx.waker()) {
                State::Waiting => return Poll::Pending,
                State::Granted => {
                    sem.waiters.take(key);
                    this.key = None;
                    return Poll::Ready(Ok(SemaphorePermit::new(sem, this.n)));
                }
                State::Closed => {
                    sem.waiters.take(key);
                    this.key = None;
                    return Poll::Ready(Err(AcquireError::closed()));
                }
                State::Free => this.key = None,
            }
        }
        if crate::coop::exhausted(cx.waker()) {
            return Poll::Pending;
        }
        if sem.closed.get() {
            crate::coop::spent_one();
            return Poll::Ready(Err(AcquireError::closed()));
        }
        if sem.waiters.is_empty() && sem.permits.get() >= this.n as isize {
            sem.permits.set(sem.permits.get() - this.n as isize);
            crate::coop::spent_one();
            return Poll::Ready(Ok(SemaphorePermit::new(sem, this.n)));
        }
        this.queue(cx)
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl fmt::Debug for Acquire<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Acquire")
            .field("permits", &self.n)
            .field("waiting", &self.key.is_some())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use core::cell::RefCell;
    use core::task::Waker;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::Rc;

    use super::*;
    use crate::sync::test_support::{Polled, hooked};

    fn leaked(permits: usize) -> &'static Semaphore {
        Box::leak(Box::new(Semaphore::new(permits)))
    }

    fn boom() -> Waker {
        hooked(
            None,
            Some(Box::new(|| panic!("a waker panicked as it was woken"))),
        )
    }

    /// A hook that drops the permit in `held`.
    fn release(held: &Rc<RefCell<Option<SemaphorePermit<'static>>>>) -> Waker {
        let held = Rc::clone(held);
        hooked(Some(Box::new(move || drop(held.borrow_mut().take()))), None)
    }

    fn held(sem: &'static Semaphore) -> Rc<RefCell<Option<SemaphorePermit<'static>>>> {
        Rc::new(RefCell::new(Some(sem.try_acquire().unwrap())))
    }

    #[test]
    fn an_acquire_whose_clone_releases_a_permit_takes_it() {
        let sem = leaked(1);
        let held = held(sem);
        let mut acquire = Box::pin(sem.acquire());
        let polled = acquire
            .as_mut()
            .poll(&mut Context::from_waker(&release(&held)));
        let Poll::Ready(Ok(permit)) = polled else {
            panic!("the acquire slept beside a free permit");
        };
        assert_eq!(permit.num_permits(), 1);
        assert_eq!(sem.available_permits(), 0);
    }

    #[test]
    fn an_acquire_whose_clone_closes_the_semaphore_fails() {
        let sem = leaked(0);
        let closer = hooked(Some(Box::new(move || sem.close())), None);
        let mut acquire = Box::pin(sem.acquire());
        let polled = acquire.as_mut().poll(&mut Context::from_waker(&closer));
        assert!(matches!(polled, Poll::Ready(Err(_))));
    }

    #[test]
    fn a_refresh_whose_clone_causes_the_grant_reports_it() {
        let sem = leaked(1);
        let held = held(sem);
        let first = Polled::new(async {});
        let mut acquire = Box::pin(sem.acquire());
        let queued = acquire
            .as_mut()
            .poll(&mut Context::from_waker(&first.waker()));
        assert!(queued.is_pending());
        let polled = acquire
            .as_mut()
            .poll(&mut Context::from_waker(&release(&held)));
        assert!(matches!(polled, Poll::Ready(Ok(_))), "the grant was missed");
    }

    #[test]
    fn a_close_whose_first_waker_panics_still_ends_every_other_wait() {
        let sem = leaked(0);
        let mut first = Box::pin(sem.acquire());
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(&boom()))
                .is_pending()
        );
        let mut second = Polled::new(sem.acquire());
        second.pending();
        assert!(catch_unwind(AssertUnwindSafe(|| sem.close())).is_err());
        assert!(sem.is_closed());
        assert!(second.ready().is_err());
        let polled = first.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(polled, Poll::Ready(Err(_))));
    }

    #[test]
    fn a_serve_whose_first_waker_panics_still_serves_the_next_waiter() {
        let sem = leaked(0);
        let mut first = Box::pin(sem.acquire());
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(&boom()))
                .is_pending()
        );
        let mut second = Polled::new(sem.acquire());
        second.pending();
        assert!(catch_unwind(AssertUnwindSafe(|| sem.add_permits(2))).is_err());
        assert_eq!(second.wakes(), 1);
        let permit = second.ready().unwrap();
        assert_eq!(permit.num_permits(), 1);
        let polled = first.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(polled, Poll::Ready(Ok(_))));
    }

    #[test]
    fn a_debt_never_shows_as_negative_permits() {
        let sem = Semaphore::new(2);
        sem.debit(5);
        assert_eq!(sem.available_permits(), 0);
        assert_eq!(sem.try_acquire().unwrap_err(), TryAcquireError::NoPermits);
        sem.add_permits(3);
        assert_eq!(sem.available_permits(), 0);
        assert!(sem.try_acquire().is_err());
        sem.add_permits(1);
        assert_eq!(sem.available_permits(), 1);
        assert_eq!(sem.forget_permits(5), 1);
        assert_eq!(sem.available_permits(), 0);
    }
}
