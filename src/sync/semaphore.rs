//! A fair, weighted counting semaphore whose waits are futures.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use super::raw_semaphore::{MAX, PermitWait, RawSemaphore};

/// A counting semaphore whose waits are futures.
///
/// A request for `n` permits takes them whenever `n` are free, even while
/// others wait; under contention [`acquire`](Self::acquire) queues. A
/// release wakes the oldest waiters the freed permits could
/// serve, stopping at the first that does not fit, so a large request at
/// the front is not skipped for smaller ones behind it. A waiter that
/// loses [`MAX_BYPASS`](super::MAX_BYPASS) times is owed a handoff: until
/// it is served, [`try_acquire`](Self::try_acquire) fails and releases go
/// to the waiters in queue order.
///
/// ```
/// use nuclei::sync::Semaphore;
///
/// let budget = Semaphore::new(4);
/// let first = budget.try_acquire(3).expect("three of four are free");
/// assert!(budget.try_acquire(2).is_none());
/// drop(first);
/// assert_eq!(budget.available_permits(), 4);
/// ```
pub struct Semaphore {
    raw: RawSemaphore,
}

impl Semaphore {
    /// The most permits a semaphore can hold: `usize::MAX >> 6`.
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

    /// The permits free right now.
    pub fn available_permits(&self) -> usize {
        self.raw.available()
    }

    /// Takes `n` permits if they are free and no handoff is owed.
    pub fn try_acquire(&self, n: usize) -> Option<SemaphorePermit<'_>> {
        self.raw
            .try_acquire(n)
            .then(|| SemaphorePermit { sem: self, n })
    }

    /// Waits for `n` permits. Uncontended, the first poll is one atomic
    /// read-modify-write. Contended, it is lock-free, not wait-free: see
    /// [Progress](super#progress).
    ///
    /// # Panics
    ///
    /// When `n` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS), which no
    /// semaphore can ever satisfy.
    pub fn acquire(&self, n: usize) -> Acquire<'_> {
        assert!(n <= MAX, "a request exceeds Semaphore::MAX_PERMITS");
        Acquire {
            sem: self,
            n,
            wait: PermitWait::new(),
        }
    }

    /// Waits for `n` permits held through an `Arc`, so the permit can
    /// outlive the borrow.
    ///
    /// # Panics
    ///
    /// When `n` exceeds [`MAX_PERMITS`](Self::MAX_PERMITS).
    pub fn acquire_owned(self: &Arc<Self>, n: usize) -> AcquireOwned {
        assert!(n <= MAX, "a request exceeds Semaphore::MAX_PERMITS");
        AcquireOwned {
            sem: Arc::clone(self),
            n,
            wait: PermitWait::new(),
        }
    }

    /// Adds `n` permits, handing them to waiters first. It makes the
    /// progress a returned permit does (see [`SemaphorePermit`]).
    ///
    /// # Panics
    ///
    /// When the semaphore would hold more than
    /// [`MAX_PERMITS`](Self::MAX_PERMITS).
    pub fn add_permits(&self, n: usize) {
        self.raw.add(n);
    }
}

impl fmt::Debug for Semaphore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Semaphore")
            .field("available_permits", &self.available_permits())
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
    n: usize,
}

impl SemaphorePermit<'_> {
    /// How many permits this holds.
    pub fn count(&self) -> usize {
        self.n
    }

    /// Keeps the permits out of the semaphore for good.
    pub fn forget(mut self) {
        self.n = 0;
    }
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.sem.raw.release(self.n);
    }
}

impl fmt::Debug for SemaphorePermit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemaphorePermit")
            .field("count", &self.n)
            .finish()
    }
}

/// Permits taken from a [`Semaphore`] held through an `Arc`. The release
/// makes the progress [`SemaphorePermit`]'s does.
#[must_use = "dropping a permit returns it at once"]
pub struct OwnedSemaphorePermit {
    sem: Arc<Semaphore>,
    n: usize,
}

impl OwnedSemaphorePermit {
    /// How many permits this holds.
    pub fn count(&self) -> usize {
        self.n
    }

    /// Keeps the permits out of the semaphore for good.
    pub fn forget(mut self) {
        self.n = 0;
    }
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        self.sem.raw.release(self.n);
    }
}

impl fmt::Debug for OwnedSemaphorePermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedSemaphorePermit")
            .field("count", &self.n)
            .finish()
    }
}

/// The future [`Semaphore::acquire`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct Acquire<'a> {
    sem: &'a Semaphore,
    n: usize,
    wait: PermitWait,
}

impl<'a> Future for Acquire<'a> {
    type Output = SemaphorePermit<'a>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<SemaphorePermit<'a>> {
        let this = self.get_mut();
        this.wait
            .poll(&this.sem.raw, this.n, cx.waker())
            .map(|()| SemaphorePermit {
                sem: this.sem,
                n: this.n,
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

/// The future [`Semaphore::acquire_owned`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct AcquireOwned {
    sem: Arc<Semaphore>,
    n: usize,
    wait: PermitWait,
}

impl Future for AcquireOwned {
    type Output = OwnedSemaphorePermit;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<OwnedSemaphorePermit> {
        let this = self.get_mut();
        this.wait
            .poll(&this.sem.raw, this.n, cx.waker())
            .map(|()| OwnedSemaphorePermit {
                sem: Arc::clone(&this.sem),
                n: this.n,
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
        let two = sem.try_acquire(2).expect("two of three are free");
        assert_eq!(sem.available_permits(), 1);
        assert!(sem.try_acquire(2).is_none());
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
            holder = sem.try_acquire(1).expect("a free permit is taken at once");
            waiter.pending();
        }
        holder
    }

    #[test]
    fn a_release_wakes_the_front_request_and_none_it_cannot_serve() {
        let sem = Semaphore::new(2);
        let held = sem.try_acquire(2).expect("free");
        let mut big = Polled::new(sem.acquire(2));
        big.pending();
        let mut small = Polled::new(sem.acquire(1));
        small.pending();
        drop(held);
        assert_eq!(big.wakes(), 1, "the front request is woken");
        assert_eq!(small.wakes(), 0, "the freed permits are all the front's");
        let big = big.ready();
        small.pending();
        drop(big);
        assert_eq!(small.wakes(), 1);
        assert_eq!(small.ready().count(), 1);
    }

    #[test]
    fn a_free_permit_is_taken_past_a_waiter_until_it_is_owed() {
        let sem = Semaphore::new(1);
        let holder = sem.try_acquire(1).expect("free");
        let mut waiter = Polled::new(sem.acquire(1));
        waiter.pending();
        let holder = owe(&sem, &mut waiter, holder);
        assert!(
            sem.try_acquire(1).is_none(),
            "nobody barges past an owed waiter"
        );
        drop(holder);
        assert_eq!(sem.available_permits(), 0, "the permit went to the waiter");
        drop(waiter.ready());
        assert!(
            sem.try_acquire(1).is_some(),
            "barging resumes once it is served"
        );
    }

    #[test]
    fn a_large_request_is_not_starved_by_small_callers() {
        let sem = Semaphore::new(2);
        let mut small = sem.try_acquire(1).expect("free");
        let other = sem.try_acquire(1).expect("free");
        let mut big = Polled::new(sem.acquire(2));
        big.pending();
        for round in 0..crate::sync::MAX_BYPASS {
            drop(small);
            assert_eq!(
                big.wakes() as u32,
                round + 1,
                "a release wakes the front whatever it asks for"
            );
            small = sem
                .try_acquire(1)
                .expect("a small caller takes the freed permit");
            big.pending();
        }
        drop(small);
        assert!(
            sem.try_acquire(1).is_none(),
            "the freed permit is kept for the owed request"
        );
        drop(other);
        assert_eq!(big.ready().count(), 2);
    }

    #[test]
    fn a_withdrawn_front_waiter_unblocks_the_one_behind() {
        let sem = Semaphore::new(2);
        let first = sem.try_acquire(1).expect("free");
        let second = sem.try_acquire(1).expect("free");
        let mut big = Polled::new(sem.acquire(2));
        big.pending();
        let mut small = Polled::new(sem.acquire(1));
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
        let holder = sem.try_acquire(1).expect("free");
        let mut owed = Polled::new(sem.acquire(1));
        owed.pending();
        let holder = owe(&sem, &mut owed, holder);
        let mut next = Polled::new(sem.acquire(1));
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
        let mut waiter = Polled::new(sem.acquire(1));
        waiter.pending();
        assert_eq!(Polled::new(sem.acquire(0)).ready().count(), 0);
        sem.add_permits(1);
        waiter.ready().forget();
        assert_eq!(sem.available_permits(), 0);
    }

    #[test]
    fn owned_permits_outlive_the_borrow() {
        let sem = Arc::new(Semaphore::new(1));
        let permit = block_on(sem.acquire_owned(1));
        let mut next = Polled::new(sem.acquire_owned(1));
        next.pending();
        drop(permit);
        assert_eq!(next.ready().count(), 1);
    }

    #[test]
    fn withdrawn_waiters_do_not_pile_up_behind_a_live_one() {
        let sem = Semaphore::new(0);
        let mut front = Polled::new(sem.acquire(1));
        front.pending();
        for _ in 0..1_000 {
            Polled::new(sem.acquire(1)).pending();
        }
        assert!(
            sem.raw.linked() <= 4,
            "{} nodes still linked",
            sem.raw.linked()
        );
        sem.add_permits(1);
        front.ready().forget();
    }

    #[test]
    #[should_panic(expected = "at most Semaphore::MAX_PERMITS")]
    fn overflowing_the_permit_count_fails_loudly() {
        Semaphore::new(Semaphore::MAX_PERMITS).add_permits(1);
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
                        let permit = block_on(sem.acquire(1 + i % 2));
                        let now = inside.fetch_add(permit.count(), StdOrdering::SeqCst);
                        assert!(now + permit.count() <= 2, "more permits out than exist");
                        inside.fetch_sub(permit.count(), StdOrdering::SeqCst);
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
