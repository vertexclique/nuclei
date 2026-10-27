//! A fair, weighted counting semaphore whose waits are futures.

use core::error::Error;
use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use super::contend::{Closed, Lock};
use super::internal;
use super::raw_semaphore::{MAX, PermitWait, RawSemaphore, observed_closed};

/// Error returned from [`Semaphore::acquire`] and the other waits for
/// permits.
///
/// A wait can only fail if the semaphore has been
/// [closed](Semaphore::close).
#[derive(Debug)]
pub struct AcquireError(());

impl AcquireError {
    fn closed() -> Self {
        Self(())
    }
}

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("semaphore closed")
    }
}

impl Error for AcquireError {}

/// Error returned from [`Semaphore::try_acquire`] and the other attempts
/// that never wait.
#[derive(Debug, PartialEq, Eq)]
pub enum TryAcquireError {
    /// The semaphore has been [closed](Semaphore::close) and cannot issue
    /// new permits.
    Closed,

    /// The semaphore has no available permits.
    NoPermits,
}

impl TryAcquireError {
    /// What a failed attempt that saw `state` means.
    fn of(state: usize) -> Self {
        if observed_closed(state) {
            Self::Closed
        } else {
            Self::NoPermits
        }
    }
}

impl fmt::Display for TryAcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("semaphore closed"),
            Self::NoPermits => f.write_str("no permits available"),
        }
    }
}

impl Error for TryAcquireError {}

/// A counting semaphore whose waits are futures.
///
/// A request for `n` permits takes them whenever `n` are free, even while
/// others wait; under contention [`acquire_many`](Self::acquire_many)
/// queues. A release wakes the oldest waiters the freed permits could
/// serve, stopping at the first that does not fit, so a large request at
/// the front is not skipped for smaller ones behind it. A waiter that
/// loses [`MAX_BYPASS`](super::MAX_BYPASS) times is owed a handoff: until
/// it is served, [`try_acquire`](Self::try_acquire) fails and releases go
/// to the waiters in queue order.
///
/// [`close`](Self::close) ends every wait with an [`AcquireError`] and makes
/// later acquires fail at once.
///
/// ```
/// use nuclei::sync::{Semaphore, TryAcquireError};
///
/// let budget = Semaphore::new(4);
/// let first = budget.try_acquire_many(3).expect("three of four are free");
/// assert_eq!(budget.try_acquire_many(2).unwrap_err(), TryAcquireError::NoPermits);
/// drop(first);
/// assert_eq!(budget.available_permits(), 4);
/// ```
pub struct Semaphore {
    raw: RawSemaphore,
}

impl Semaphore {
    /// The most permits a semaphore can hold: `usize::MAX >> 7`. The state
    /// word keeps seven low bits for the wait queue and the closed flag, so
    /// this is `2^57 - 1` on a 64-bit target and `2^25 - 1` on a 32-bit one.
    pub const MAX_PERMITS: usize = MAX;

    loom_const_fn! {
        /// A semaphore holding `permits` permits.
        ///
        /// # Panics
        ///
        /// When `permits` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS).
        pub fn new(permits: usize) -> Self {
            Self { raw: RawSemaphore::new(permits) }
        }
    }

    loom_const_fn! {
        /// A semaphore holding `permits` permits, usable in a `static`.
        ///
        /// # Panics
        ///
        /// When `permits` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS).
        pub fn const_new(permits: usize) -> Self {
            Self { raw: RawSemaphore::new(permits) }
        }
    }

    /// The permits free right now.
    pub fn available_permits(&self) -> usize {
        self.raw.available()
    }

    /// Adds `n` new permits, handing them to waiters first. It makes the
    /// progress a returned permit does (see [`SemaphorePermit`]). Permits
    /// added to a closed semaphore are counted, but nothing is handed out.
    ///
    /// # Panics
    ///
    /// When the semaphore would hold more than
    /// [`MAX_PERMITS`](Self::MAX_PERMITS).
    pub fn add_permits(&self, n: usize) {
        self.raw.add(n);
    }

    /// Removes up to `n` free permits for good and returns how many it
    /// removed, which is fewer than `n` when fewer are free. Permits held by
    /// outstanding [`SemaphorePermit`]s are not touched.
    pub fn forget_permits(&self, n: usize) -> usize {
        self.raw.forget(n)
    }

    /// Waits for one permit. Uncontended, the first poll is one atomic
    /// read-modify-write. Contended, it is lock-free, not wait-free: see
    /// [Progress](super#progress).
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
    pub fn acquire_many(&self, n: u32) -> Acquire<'_> {
        Acquire {
            sem: self,
            n: checked_permits(n),
            wait: PermitWait::new(),
        }
    }

    /// Waits for one permit through an `Arc`, so the permit can outlive the
    /// borrow.
    ///
    /// Completes with an [`AcquireError`] once the semaphore is
    /// [closed](Self::close).
    pub fn acquire_owned(self: Arc<Self>) -> AcquireOwned {
        self.acquire_many_owned(1)
    }

    /// Waits for `n` permits through an `Arc`, so the permits can outlive
    /// the borrow.
    ///
    /// # Panics
    ///
    /// When `n` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS), as for
    /// [`acquire_many`](Self::acquire_many).
    pub fn acquire_many_owned(self: Arc<Self>, n: u32) -> AcquireOwned {
        AcquireOwned {
            sem: self,
            n: checked_permits(n),
            wait: PermitWait::new(),
        }
    }

    /// Takes one permit if it is free and no handoff is owed.
    pub fn try_acquire(&self) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        self.try_acquire_many(1)
    }

    /// Takes `n` permits together if they are free and no handoff is owed.
    ///
    /// # Panics
    ///
    /// When `n` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS), as for
    /// [`acquire_many`](Self::acquire_many).
    pub fn try_acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, TryAcquireError> {
        let permits = checked_permits(n);
        self.raw
            .try_take(permits)
            .map(|()| SemaphorePermit { sem: self, permits })
            .map_err(TryAcquireError::of)
    }

    /// Takes one permit through an `Arc` if it is free and no handoff is
    /// owed.
    pub fn try_acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.try_acquire_many_owned(1)
    }

    /// Takes `n` permits together through an `Arc` if they are free and no
    /// handoff is owed.
    ///
    /// # Panics
    ///
    /// When `n` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS), as for
    /// [`acquire_many`](Self::acquire_many).
    pub fn try_acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        let permits = checked_permits(n);
        match self.raw.try_take(permits) {
            Ok(()) => Ok(OwnedSemaphorePermit { sem: self, permits }),
            Err(seen) => Err(TryAcquireError::of(seen)),
        }
    }

    /// [`acquire`](Self::acquire) for a thread that may block: it parks the
    /// calling thread until the permit is free. On a nuclei shard (inside one of its tasks) it does not park the thread: it runs a nested `block_on`, so the shard keeps running its other tasks meanwhile.
    /// Inside another runtime's task it blocks that task's executor thread,
    /// so do not.
    ///
    /// # Panics
    ///
    /// On `wasm32`, where no other thread can return a permit, when the
    /// permit is not free: the call could never end.
    pub fn blocking_acquire(&self) -> Result<SemaphorePermit<'_>, AcquireError> {
        internal::blocking(self.acquire())
    }

    /// [`acquire_many`](Self::acquire_many) for a thread that may block: see
    /// [`blocking_acquire`](Self::blocking_acquire).
    ///
    /// # Panics
    ///
    /// As for [`blocking_acquire`](Self::blocking_acquire), and when `n`
    /// exceeds [`MAX_PERMITS`](Self::MAX_PERMITS).
    pub fn blocking_acquire_many(&self, n: u32) -> Result<SemaphorePermit<'_>, AcquireError> {
        internal::blocking(self.acquire_many(n))
    }

    /// [`acquire_owned`](Self::acquire_owned) for a thread that may block:
    /// see [`blocking_acquire`](Self::blocking_acquire).
    ///
    /// # Panics
    ///
    /// As for [`blocking_acquire`](Self::blocking_acquire).
    pub fn blocking_acquire_owned(self: Arc<Self>) -> Result<OwnedSemaphorePermit, AcquireError> {
        internal::blocking(self.acquire_owned())
    }

    /// [`acquire_many_owned`](Self::acquire_many_owned) for a thread that may
    /// block: see [`blocking_acquire`](Self::blocking_acquire).
    ///
    /// # Panics
    ///
    /// As for [`blocking_acquire`](Self::blocking_acquire), and when `n`
    /// exceeds [`MAX_PERMITS`](Self::MAX_PERMITS).
    pub fn blocking_acquire_many_owned(
        self: Arc<Self>,
        n: u32,
    ) -> Result<OwnedSemaphorePermit, AcquireError> {
        internal::blocking(self.acquire_many_owned(n))
    }

    /// Closes the semaphore. Every wait still queued ends in an
    /// [`AcquireError`], and every later acquire fails at once: `try_acquire`
    /// and its siblings return [`TryAcquireError::Closed`], whatever permits
    /// are free. A wait that was handed its permits before the close keeps
    /// them. Permits returned or added afterwards are counted, and
    /// [`available_permits`](Self::available_permits) shows them, but none is
    /// ever handed out. Closing twice does nothing.
    ///
    /// It makes the progress a returned permit does (see
    /// [`SemaphorePermit`]).
    pub fn close(&self) {
        self.raw.close();
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        self.raw.is_closed()
    }
}

/// `n` as a permit count, or a panic when no semaphore could ever satisfy it.
#[expect(
    clippy::panic,
    reason = "documented under `# Panics` of the calls that take permits"
)]
fn checked_permits(n: u32) -> usize {
    match usize::try_from(n) {
        Ok(permits) if permits <= MAX => permits,
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
///
/// The release is one atomic read-modify-write when no waiter needs telling.
/// Otherwise it is lock-free, not wait-free: it returns after one pass over
/// the queue, plus at most one for each operation of another thread that
/// lands on the queue meanwhile (see [Progress](super#progress)).
#[must_use = "dropping a permit returns it at once"]
pub struct SemaphorePermit<'a> {
    sem: &'a Semaphore,
    permits: usize,
}

impl<'a> SemaphorePermit<'a> {
    /// Keeps the permits out of the semaphore for good.
    pub fn forget(mut self) {
        self.permits = 0;
    }

    /// Merges `other` into this permit, so one drop returns both.
    ///
    /// # Panics
    ///
    /// When the permits come from different semaphores, or their total
    /// overflows `usize`.
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

    /// Splits `n` permits off this one, or returns `None` when it holds
    /// fewer than `n`.
    pub fn split(&mut self, n: usize) -> Option<Self> {
        if n > self.permits {
            return None;
        }
        self.permits -= n;
        Some(Self {
            sem: self.sem,
            permits: n,
        })
    }

    /// The semaphore these permits came from.
    pub fn semaphore(&self) -> &'a Semaphore {
        self.sem
    }

    /// How many permits this holds.
    pub fn num_permits(&self) -> usize {
        self.permits
    }

    /// How many permits this holds; the same as
    /// [`num_permits`](Self::num_permits).
    pub fn count(&self) -> usize {
        self.permits
    }
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.sem.raw.release(self.permits);
    }
}

impl fmt::Debug for SemaphorePermit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemaphorePermit")
            .field("permits", &self.permits)
            .finish()
    }
}

/// Permits taken from a [`Semaphore`] held through an `Arc`. The release
/// makes the progress [`SemaphorePermit`]'s does.
#[must_use = "dropping a permit returns it at once"]
pub struct OwnedSemaphorePermit {
    sem: Arc<Semaphore>,
    permits: usize,
}

impl OwnedSemaphorePermit {
    /// Keeps the permits out of the semaphore for good.
    pub fn forget(mut self) {
        self.permits = 0;
    }

    /// Merges `other` into this permit, so one drop returns both.
    ///
    /// # Panics
    ///
    /// When the permits come from different semaphores, or their total
    /// overflows `usize`.
    #[track_caller]
    pub fn merge(&mut self, mut other: Self) {
        assert!(
            Arc::ptr_eq(&self.sem, &other.sem),
            "merging permits from different semaphore instances"
        );
        // Both are of one semaphore, whose permits never pass `MAX_PERMITS`.
        self.permits += other.permits;
        other.permits = 0;
    }

    /// Splits `n` permits off this one, or returns `None` when it holds
    /// fewer than `n`.
    pub fn split(&mut self, n: usize) -> Option<Self> {
        if n > self.permits {
            return None;
        }
        self.permits -= n;
        Some(Self {
            sem: Arc::clone(&self.sem),
            permits: n,
        })
    }

    /// The semaphore these permits came from.
    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.sem
    }

    /// How many permits this holds.
    pub fn num_permits(&self) -> usize {
        self.permits
    }

    /// How many permits this holds; the same as
    /// [`num_permits`](Self::num_permits).
    pub fn count(&self) -> usize {
        self.permits
    }
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        self.sem.raw.release(self.permits);
    }
}

impl fmt::Debug for OwnedSemaphorePermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedSemaphorePermit")
            .field("permits", &self.permits)
            .finish()
    }
}

/// The future [`Semaphore::acquire`] and [`Semaphore::acquire_many`] return.
///
/// Dropping it withdraws the wait, and passes on permits handed to it
/// meanwhile.
#[must_use = "futures do nothing unless polled"]
pub struct Acquire<'a> {
    sem: &'a Semaphore,
    n: usize,
    wait: PermitWait,
}

impl<'a> Future for Acquire<'a> {
    type Output = Result<SemaphorePermit<'a>, AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.wait
            .poll_closable(&this.sem.raw, this.n, cx.waker())
            .map(|taken| {
                taken
                    .map(|()| SemaphorePermit {
                        sem: this.sem,
                        permits: this.n,
                    })
                    .map_err(|Closed| AcquireError::closed())
            })
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        self.wait.cancel(&self.sem.raw, self.n);
    }
}

impl fmt::Debug for Acquire<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Acquire").field("n", &self.n).finish()
    }
}

/// The future [`Semaphore::acquire_owned`] and
/// [`Semaphore::acquire_many_owned`] return.
#[must_use = "futures do nothing unless polled"]
pub struct AcquireOwned {
    sem: Arc<Semaphore>,
    n: usize,
    wait: PermitWait,
}

impl Future for AcquireOwned {
    type Output = Result<OwnedSemaphorePermit, AcquireError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.wait
            .poll_closable(&this.sem.raw, this.n, cx.waker())
            .map(|taken| {
                taken
                    .map(|()| OwnedSemaphorePermit {
                        sem: Arc::clone(&this.sem),
                        permits: this.n,
                    })
                    .map_err(|Closed| AcquireError::closed())
            })
    }
}

impl Drop for AcquireOwned {
    fn drop(&mut self) {
        self.wait.cancel(&self.sem.raw, self.n);
    }
}

impl fmt::Debug for AcquireOwned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcquireOwned").field("n", &self.n).finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::{Polled, block_on};
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};

    const ITERATIONS: usize = if cfg!(miri) { 20 } else { 2_000 };

    #[test]
    fn uncontended_permits_come_and_go() {
        let sem = Semaphore::new(3);
        let two = sem.try_acquire_many(2).expect("two of three are free");
        assert_eq!(sem.available_permits(), 1);
        assert_eq!(
            sem.try_acquire_many(2).unwrap_err(),
            TryAcquireError::NoPermits
        );
        drop(two);
        assert_eq!(sem.available_permits(), 3);
    }

    /// A semaphore of one permit whose waiter has been passed over
    /// `MAX_BYPASS` times, so it is owed the permit; returns the holder.
    fn owe<'a>(
        sem: &'a Semaphore,
        waiter: &mut Polled<Acquire<'a>>,
        mut holder: SemaphorePermit<'a>,
    ) -> SemaphorePermit<'a> {
        for round in 0..crate::sync::MAX_BYPASS {
            drop(holder);
            assert_eq!(waiter.wakes() as u32, round + 1, "a release nudges it");
            holder = sem.try_acquire().expect("a free permit is taken at once");
            waiter.pending();
        }
        holder
    }

    #[test]
    fn a_release_wakes_the_front_request_and_none_it_cannot_serve() {
        let sem = Semaphore::new(2);
        let held = sem.try_acquire_many(2).expect("free");
        let mut big = Polled::new(sem.acquire_many(2));
        big.pending();
        let mut small = Polled::new(sem.acquire());
        small.pending();
        drop(held);
        assert_eq!(big.wakes(), 1, "the front request is woken");
        assert_eq!(small.wakes(), 0, "the freed permits are all the front's");
        let big = big.ready().expect("open");
        small.pending();
        drop(big);
        assert_eq!(small.wakes(), 1);
        assert_eq!(small.ready().expect("open").num_permits(), 1);
    }

    #[test]
    fn a_free_permit_is_taken_past_a_waiter_until_it_is_owed() {
        let sem = Semaphore::new(1);
        let holder = sem.try_acquire().expect("free");
        let mut waiter = Polled::new(sem.acquire());
        waiter.pending();
        let holder = owe(&sem, &mut waiter, holder);
        assert_eq!(
            sem.try_acquire().unwrap_err(),
            TryAcquireError::NoPermits,
            "nobody barges past an owed waiter"
        );
        drop(holder);
        assert_eq!(sem.available_permits(), 0, "the permit went to the waiter");
        drop(waiter.ready());
        assert!(
            sem.try_acquire().is_ok(),
            "barging resumes once it is served"
        );
    }

    #[test]
    fn a_large_request_is_not_starved_by_small_callers() {
        let sem = Semaphore::new(2);
        let mut small = sem.try_acquire().expect("free");
        let other = sem.try_acquire().expect("free");
        let mut big = Polled::new(sem.acquire_many(2));
        big.pending();
        for round in 0..crate::sync::MAX_BYPASS {
            drop(small);
            assert_eq!(
                big.wakes() as u32,
                round + 1,
                "a release wakes the front whatever it asks for"
            );
            small = sem
                .try_acquire()
                .expect("a small caller takes the freed permit");
            big.pending();
        }
        drop(small);
        assert!(
            sem.try_acquire().is_err(),
            "the freed permit is kept for the owed request"
        );
        drop(other);
        assert_eq!(big.ready().expect("open").num_permits(), 2);
    }

    #[test]
    fn a_withdrawn_front_waiter_unblocks_the_one_behind() {
        let sem = Semaphore::new(2);
        let first = sem.try_acquire().expect("free");
        let second = sem.try_acquire().expect("free");
        let mut big = Polled::new(sem.acquire_many(2));
        big.pending();
        let mut small = Polled::new(sem.acquire());
        small.pending();
        drop(first);
        assert_eq!(small.wakes(), 0, "one permit is not enough for the front");
        drop(big);
        assert_eq!(small.wakes(), 1);
        drop(small.ready());
        drop(second);
        assert_eq!(sem.available_permits(), 2);
    }

    #[test]
    fn a_handoff_dropped_unseen_passes_its_permit_on() {
        let sem = Semaphore::new(1);
        let holder = sem.try_acquire().expect("free");
        let mut owed = Polled::new(sem.acquire());
        owed.pending();
        let holder = owe(&sem, &mut owed, holder);
        let mut next = Polled::new(sem.acquire());
        next.pending();
        drop(holder);
        drop(owed);
        assert_eq!(next.wakes(), 1, "the dropped handoff moved on");
        drop(next.ready());
        assert_eq!(sem.available_permits(), 1);
    }

    #[test]
    fn a_request_for_nothing_never_waits() {
        let sem = Semaphore::new(0);
        let mut waiter = Polled::new(sem.acquire());
        waiter.pending();
        assert_eq!(
            Polled::new(sem.acquire_many(0))
                .ready()
                .expect("open")
                .num_permits(),
            0
        );
        sem.add_permits(1);
        waiter.ready().expect("open").forget();
        assert_eq!(sem.available_permits(), 0);
    }

    #[test]
    fn owned_permits_outlive_the_borrow() {
        let sem = Arc::new(Semaphore::new(1));
        let permit = block_on(Arc::clone(&sem).acquire_owned()).expect("open");
        let mut next = Polled::new(Arc::clone(&sem).acquire_owned());
        next.pending();
        drop(permit);
        assert_eq!(next.ready().expect("open").num_permits(), 1);
    }

    #[test]
    fn withdrawn_waiters_do_not_pile_up_behind_a_live_one() {
        let sem = Semaphore::new(0);
        let mut front = Polled::new(sem.acquire());
        front.pending();
        for _ in 0..1_000 {
            Polled::new(sem.acquire()).pending();
        }
        assert!(
            sem.raw.linked() <= 4,
            "{} nodes still linked",
            sem.raw.linked()
        );
        sem.add_permits(1);
        front.ready().expect("open").forget();
    }

    #[test]
    #[should_panic(expected = "at most Semaphore::MAX_PERMITS")]
    fn overflowing_the_permit_count_fails_loudly() {
        Semaphore::new(Semaphore::MAX_PERMITS).add_permits(1);
    }

    #[test]
    fn closing_ends_every_queued_wait_and_refuses_later_ones() {
        let sem = Semaphore::new(1);
        let held = sem.try_acquire().expect("free");
        let mut first = Polled::new(sem.acquire());
        first.pending();
        let mut second = Polled::new(sem.acquire_many(1));
        second.pending();
        assert!(!sem.is_closed());
        sem.close();
        assert!(sem.is_closed());
        assert_eq!((first.wakes(), second.wakes()), (1, 1));
        assert_eq!(first.ready().unwrap_err().to_string(), "semaphore closed");
        assert!(second.ready().is_err());
        assert!(
            Polled::new(sem.acquire()).ready().is_err(),
            "a later wait fails at once"
        );
        assert_eq!(sem.try_acquire().unwrap_err(), TryAcquireError::Closed);
        drop(held);
        assert_eq!(sem.available_permits(), 1, "the returned permit counts");
        assert_eq!(
            sem.try_acquire().unwrap_err(),
            TryAcquireError::Closed,
            "but is not handed out"
        );
        sem.add_permits(2);
        assert_eq!(sem.available_permits(), 3, "added permits count too");
        sem.close();
    }

    #[test]
    fn a_wait_handed_its_permit_before_the_close_keeps_it() {
        let sem = Semaphore::new(1);
        let held = sem.try_acquire().expect("free");
        let mut waiter = Polled::new(sem.acquire());
        waiter.pending();
        // Owe it the permit, then hand it over by the release.
        let held = owe(&sem, &mut waiter, held);
        drop(held);
        sem.close();
        let permit = waiter.ready().expect("the permit was its before the close");
        drop(permit);
        assert_eq!(sem.available_permits(), 1);
    }

    #[test]
    fn a_closed_wait_dropped_unseen_returns_nothing_twice() {
        let sem = Semaphore::new(1);
        let held = sem.try_acquire().expect("free");
        let mut waiter = Polled::new(sem.acquire());
        waiter.pending();
        sem.close();
        drop(waiter);
        drop(held);
        assert_eq!(sem.available_permits(), 1, "no permit made up");
    }

    #[test]
    fn forgetting_permits_takes_only_free_ones() {
        let sem = Semaphore::new(3);
        let held = sem.try_acquire().expect("free");
        assert_eq!(sem.forget_permits(5), 2);
        assert_eq!(sem.available_permits(), 0);
        assert_eq!(sem.forget_permits(0), 0);
        drop(held);
        assert_eq!(sem.available_permits(), 1);
        sem.close();
        assert_eq!(sem.forget_permits(1), 1, "forgetting keeps the closed flag");
        assert!(sem.is_closed());
    }

    #[test]
    fn permits_merge_and_split() {
        let sem = Semaphore::new(5);
        let mut all = sem.try_acquire_many(2).expect("free");
        all.merge(sem.try_acquire_many(3).expect("free"));
        assert_eq!(all.num_permits(), 5);
        let part = all.split(2).expect("held");
        assert_eq!((all.num_permits(), part.num_permits()), (3, 2));
        assert!(all.split(4).is_none());
        assert!(core::ptr::eq(all.semaphore(), &sem));
        drop(part);
        assert_eq!(sem.available_permits(), 2);
        drop(all);
        assert_eq!(sem.available_permits(), 5);
    }

    #[test]
    #[should_panic(expected = "different semaphore instances")]
    fn merging_permits_of_two_semaphores_fails_loudly() {
        let (a, b) = (Semaphore::new(1), Semaphore::new(1));
        let mut one = a.try_acquire().expect("free");
        one.merge(b.try_acquire().expect("free"));
    }

    #[test]
    fn threads_contending_never_exceed_the_permits() {
        let sem = Arc::new(Semaphore::new(2));
        let inside = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (sem, inside) = (Arc::clone(&sem), Arc::clone(&inside));
                std::thread::spawn(move || {
                    for i in 0..ITERATIONS {
                        let permit = block_on(sem.acquire_many(1 + (i % 2) as u32)).expect("open");
                        let now = inside.fetch_add(permit.num_permits(), StdOrdering::SeqCst);
                        assert!(
                            now + permit.num_permits() <= 2,
                            "more permits out than exist"
                        );
                        inside.fetch_sub(permit.num_permits(), StdOrdering::SeqCst);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("worker");
        }
        assert_eq!(sem.available_permits(), 2);
    }
}
