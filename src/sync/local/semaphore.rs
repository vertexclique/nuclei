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

use super::waiters::{Key, State, Waiters};
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
    permits: Cell<usize>,
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
            permits: Cell::new(permits),
            closed: Cell::new(false),
            waiters: Waiters::new(),
        }
    }

    /// The permits free right now, including the ones kept for the front
    /// waiter until its request fits.
    pub fn available_permits(&self) -> usize {
        self.permits.get()
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
        let total = self.permits.get().saturating_add(n);
        assert!(
            total <= Self::MAX_PERMITS,
            "a semaphore cannot hold more than Semaphore::MAX_PERMITS"
        );
        self.permits.set(total);
        self.serve();
    }

    /// Removes up to `n` free permits for good and returns how many it
    /// removed, which is fewer than `n` when fewer are free. Permits held by
    /// outstanding [`SemaphorePermit`]s are not touched.
    pub fn forget_permits(&self, n: usize) -> usize {
        let removed = n.min(self.permits.get());
        self.permits.set(self.permits.get() - removed);
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
        if !self.waiters.is_empty() || self.permits.get() < permits {
            return Err(TryAcquireError::NoPermits);
        }
        self.permits.set(self.permits.get() - permits);
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
        while let Some(waker) = self.waiters.close_front() {
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// Takes `permits` back and serves the line.
    pub(crate) fn release(&self, permits: usize) {
        self.permits.set(self.permits.get().saturating_add(permits));
        self.serve();
    }

    /// Grants the front waiters while their requests fit. Each waker is woken
    /// outside any borrow, and the state is read again after the wake.
    fn serve(&self) {
        if self.closed.get() {
            return;
        }
        while let Some((need, waker)) = self.waiters.grant_front_if(self.permits.get()) {
            self.permits.set(self.permits.get() - need);
            if let Some(waker) = waker {
                waker.wake();
            }
        }
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
    pub(crate) fn cancel(&mut self) {
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
        if sem.waiters.is_empty() && sem.permits.get() >= this.n {
            sem.permits.set(sem.permits.get() - this.n);
            crate::coop::spent_one();
            return Poll::Ready(Ok(SemaphorePermit::new(sem, this.n)));
        }
        this.key = Some(sem.waiters.push(this.n, cx.waker().clone()));
        Poll::Pending
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
