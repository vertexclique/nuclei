//! A first-come, first-served semaphore: the bounded channel's capacity.
//!
//! [`RawSemaphore`](super::raw_semaphore::RawSemaphore) lets a caller take
//! what is free past queued waiters, which suits a lock. The established
//! channel API serves its slots the other way: in the order the requests
//! began to wait, a request for many slots included, and a later request
//! never gets ahead of an earlier one, however small it is. This semaphore
//! keeps that order with the parts every primitive here shares: the state
//! word, arrival stack and drain role of the [`WaitQueue`], a waiting
//! [`List`], and the waiter nodes.
//!
//! - **Nobody passes a waiter.** An attempt that does not queue takes its
//!   permits only while no waiter is queued (the queue's [`QUEUED`] bit is
//!   down); otherwise it fails, or it queues behind the others.
//! - **Permits go to the front, in order.** A drain pass moves free permits
//!   out of the state word into the queue's set-aside, for the front waiter,
//!   and grants it once the set-aside covers its whole request. A request
//!   that is not covered yet stays at the front and keeps what was set aside
//!   for it, so the free count does not show those permits, and nothing
//!   behind it is served first.
//! - **A waiter that leaves passes its share on.** The set-aside belongs to
//!   the queue, not to a node, so what a withdrawn front waiter was assigned
//!   stays for the next one. A waiter granted just as it was dropped gives
//!   its permits back through [`release`](FairSemaphore::release), which
//!   hands them on the same way. When nobody is left waiting, the set-aside
//!   goes back to the state word.
//! - **Closing ends every wait.** The pass that sees the closed bit grants
//!   every waiter a [`CLOSED_GRANT`] and returns the set-aside to the state
//!   word, where it counts but is never handed out.
//!
//! So the permits free in the word, plus the set-aside, plus the permits of
//! every granted wait and of whoever holds them, are always the permits the
//! semaphore was made with.
//!
//! # Progress
//!
//! [`try_acquire`](FairSemaphore::try_acquire) is one compare-and-swap loop
//! on the state word that retries only when another thread changed the word:
//! lock-free. A release is one `fetch_add` when nobody waits; otherwise it
//! serves the queue, or leaves its request with the thread that does, and is
//! lock-free as every drain is (see `queue`). Each poll of a wait is
//! lock-free the same way. When a waiter is granted is not a progress bound
//! of its own calls: whichever thread holds the drain role while permits are
//! free grants it, so a thread the scheduler stops inside a pass delays that
//! grant until it runs again, and keeps the `QUEUED` bit up meanwhile, so an
//! attempt that does not queue fails then even if the last waiter has left.
//! No call ever waits for the role.

#![allow(unsafe_code)]

use core::task::{Poll, Waker};

use super::contend::{CLOSED_GRANT, Closed};
use super::internal::{Ordering, UnsafeCell};
use super::list::List;
use super::queue::{Arrivals, Policy, QUEUED, WaitQueue, wants_drain};
use super::raw_semaphore::{MAX, SHIFT, close_step, closed_in, observed_closed};
use super::waiter::{Cancel, Wait, WakeList};

/// What only the drain role's holder touches.
struct Waiting {
    list: List,
    /// Permits taken out of the state word for the front waiter and not yet
    /// granted. At most the front waiter's request while it waits; zero
    /// whenever the list is empty after a pass.
    set_aside: usize,
}

/// A weighted semaphore that serves its waiters strictly in arrival order.
pub(super) struct FairSemaphore {
    queue: WaitQueue,
    waiting: UnsafeCell<Waiting>,
}

// SAFETY: the waiting list and the set-aside are touched only by the drain
// role's holder.
unsafe impl Send for FairSemaphore {}
// SAFETY: as above.
unsafe impl Sync for FairSemaphore {}

impl FairSemaphore {
    /// A semaphore holding `permits` permits.
    ///
    /// # Panics
    ///
    /// When `permits` exceeds [`Semaphore::MAX_PERMITS`](super::Semaphore::MAX_PERMITS).
    pub(super) fn new(permits: usize) -> Self {
        assert!(
            permits <= MAX,
            "a semaphore holds at most Semaphore::MAX_PERMITS permits"
        );
        Self {
            queue: WaitQueue::without_spin(permits << SHIFT),
            waiting: UnsafeCell::new(Waiting {
                list: List::new(),
                set_aside: 0,
            }),
        }
    }

    /// The permits free right now: neither held, nor set aside for a waiter.
    pub(super) fn available(&self) -> usize {
        self.queue.state.load(Ordering::Acquire) >> SHIFT
    }

    /// Whether [`close`](Self::close) was called.
    pub(super) fn is_closed(&self) -> bool {
        closed_in(self.queue.state.load(Ordering::Acquire))
    }

    /// Takes `n` permits if they are free and nobody waits. `n == 0`
    /// succeeds unless the semaphore is closed. On failure, the state word
    /// it saw, which says whether the semaphore closed (see
    /// [`observed_closed`]).
    #[inline]
    pub(super) fn try_acquire(&self, n: usize) -> Result<(), usize> {
        let state = &self.queue.state;
        let mut current = state.load(Ordering::Relaxed);
        loop {
            if closed_in(current) {
                return Err(current);
            }
            if n == 0 {
                return Ok(());
            }
            if current & QUEUED != 0 || current >> SHIFT < n {
                return Err(current);
            }
            match state.compare_exchange_weak(
                current,
                current - (n << SHIFT),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    /// Returns `n` permits, handing them to the waiters first.
    ///
    /// It cannot overflow: the permits were in the semaphore before. A
    /// waiter queueing concurrently either is seen by the pass this asks
    /// for, or reads the permits in the pass its own enqueue runs.
    #[inline]
    pub(super) fn release(&self, n: usize) {
        if n == 0 {
            return;
        }
        let before = self.queue.state.fetch_add(n << SHIFT, Ordering::AcqRel);
        if wants_drain(before) {
            self.release_wakes();
        }
    }

    /// The pass a release asks for, out of line: inlined, the drain would
    /// make every release, the one-instruction kind too, pay for the
    /// registers it saves.
    #[inline(never)]
    fn release_wakes(&self) {
        self.released();
    }

    /// Closes the semaphore: every waiter ends in [`Closed`], and nothing is
    /// admitted from now on. Closing twice does nothing.
    pub(super) fn close(&self) {
        self.transition(close_step);
    }

    /// Moves up to `want` free permits out of the state word for the front
    /// waiter; none once the semaphore closed, whose pass ends every wait.
    fn take_up_to(&self, want: usize) -> usize {
        let state = &self.queue.state;
        let mut current = state.load(Ordering::Acquire);
        loop {
            let take = (current >> SHIFT).min(want);
            if take == 0 || closed_in(current) {
                return 0;
            }
            match state.compare_exchange_weak(
                current,
                current - (take << SHIFT),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return take,
                Err(actual) => current = actual,
            }
        }
    }

    /// Puts what was set aside back into the state word.
    fn return_set_aside(&self, set_aside: &mut usize) {
        if *set_aside != 0 {
            let back = probe::handed_on(probe::Site::ReturnedToWord, *set_aside);
            self.queue.state.fetch_add(back << SHIFT, Ordering::AcqRel);
            *set_aside = 0;
        }
    }
}

impl Policy for FairSemaphore {
    fn queue(&self) -> &WaitQueue {
        &self.queue
    }

    fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, _wake_front: bool) -> usize {
        let queue = &self.queue;
        // SAFETY: only the drain role's holder runs a pass.
        self.waiting.with_mut(|waiting| unsafe {
            let Waiting { list, set_aside } = &mut *waiting;
            list.absorb(arrivals, queue);
            list.tidy(queue);
            if self.is_closed() {
                while let Some(node) = list.front(queue) {
                    list.pop_front();
                    node.as_ref().set_payload(CLOSED_GRANT);
                    woken.grant(node);
                }
            } else {
                while let Some(node) = list.front(queue) {
                    let need = node.as_ref().payload();
                    if *set_aside < need {
                        *set_aside += self.take_up_to(need - *set_aside);
                        if *set_aside < need {
                            // The front keeps its share and waits for more;
                            // a release while this pass runs asks for another.
                            return 0;
                        }
                    }
                    list.pop_front();
                    if woken.grant(node) {
                        *set_aside -= need;
                    } else {
                        // It withdrew as it was granted: its share stays set
                        // aside for the next waiter.
                        *set_aside -= need - probe::handed_on(probe::Site::WithdrewAtGrant, need);
                    }
                }
            }
            self.return_set_aside(set_aside);
            QUEUED
        })
    }
}

impl Drop for FairSemaphore {
    fn drop(&mut self) {
        let queue = &self.queue;
        // SAFETY: `&mut self` rules out a concurrent pass.
        self.waiting
            .with_mut(|waiting| unsafe { (*waiting).list.clear(queue) });
    }
}

/// A future's wait for permits on a [`FairSemaphore`].
pub(super) struct FairWait {
    wait: Wait,
}

impl FairWait {
    pub(super) const fn new() -> Self {
        Self { wait: Wait::new() }
    }

    /// Ready once `n` permits are the caller's, or with [`Closed`] when the
    /// semaphore closed before they were. A first poll takes them at once if
    /// nobody waits; otherwise it queues, and later polls only look at the
    /// node.
    ///
    /// A first poll that completes spends one operation of the poll's budget
    /// (see `coop`); with the budget spent, permits it took go back and it
    /// yields. A grant to a queued wait spends nothing: that wait yielded.
    pub(super) fn poll(
        &mut self,
        sem: &FairSemaphore,
        n: usize,
        waker: &Waker,
    ) -> Poll<Result<(), Closed>> {
        self.poll_admitted(sem, n, waker, || crate::coop::spend(waker))
    }

    /// [`poll`](Self::poll) with `admit` in place of the budget: asked once
    /// a first poll would complete, and a `false` undoes it and answers
    /// `Pending`.
    pub(super) fn poll_admitted(
        &mut self,
        sem: &FairSemaphore,
        n: usize,
        waker: &Waker,
        admit: impl FnOnce() -> bool,
    ) -> Poll<Result<(), Closed>> {
        let granted = if self.wait.is_queued() {
            self.wait.poll(&sem.queue, Some(waker))
        } else {
            match sem.try_acquire(n) {
                Ok(()) => {
                    if admit() {
                        return Poll::Ready(Ok(()));
                    }
                    sem.release(n);
                    return Poll::Pending;
                }
                Err(seen) if observed_closed(seen) => {
                    return if admit() {
                        Poll::Ready(Err(Closed))
                    } else {
                        Poll::Pending
                    };
                }
                Err(_) => {}
            }
            sem.enqueue(&mut self.wait, n, waker);
            self.wait.poll(&sem.queue, None)
        };
        granted.map(|payload| {
            if payload == CLOSED_GRANT {
                Err(Closed)
            } else {
                Ok(())
            }
        })
    }

    /// Leaves the queue: the future was dropped before it completed. A grant
    /// that got there first is given back, so the next waiter gets it.
    pub(super) fn cancel(&mut self, sem: &FairSemaphore, n: usize) {
        match self.wait.cancel(&sem.queue) {
            Cancel::Idle => {}
            Cancel::Withdrawn(_) => sem.withdrawn(false),
            // A grant made because the semaphore closed carries nothing.
            Cancel::Granted(payload) if payload == CLOSED_GRANT => {}
            Cancel::Granted(_) => {
                sem.release(probe::handed_on(probe::Site::GrantedThenDropped, n));
            }
        }
    }
}

/// The places a share of permits that a leaving waiter held is handed on.
/// Outside loom each one hands on the whole share and costs nothing.
#[cfg(not(all(loom, test)))]
mod probe {
    pub(super) enum Site {
        GrantedThenDropped,
        WithdrewAtGrant,
        ReturnedToWord,
    }

    #[inline(always)]
    pub(super) fn handed_on(_site: Site, share: usize) -> usize {
        share
    }
}

/// Loom-only instruments of the handshake between a grant and a waiter that
/// leaves: what the models count to show they reach each place a share is
/// handed on, and the one guard their calibration takes away. Loom runs a
/// model's threads as coroutines on the test's own thread, so these are per
/// test.
#[cfg(all(loom, test))]
pub(super) mod probe {
    use core::cell::Cell;

    /// Where a share is handed on.
    pub(super) enum Site {
        /// A wait granted, then dropped before it looked: its permits go back
        /// through a release, which hands them to the next waiter.
        GrantedThenDropped,
        /// A front waiter withdrew as a pass granted it: its share stays set
        /// aside for the next one.
        WithdrewAtGrant,
        /// The queue emptied or the semaphore closed: the set-aside goes back
        /// to the state word.
        ReturnedToWord,
    }

    std::thread_local! {
        /// How often each site handed a share on, in the order of [`Site`].
        static COUNTS: Cell<[usize; 3]> = const { Cell::new([0; 3]) };
        /// The calibration's switch: every share is dropped instead of handed on.
        static DROP_PASS_ON: Cell<bool> = const { Cell::new(false) };
    }

    /// How much of `share` the site hands on: all of it, unless the
    /// calibration's mutation is on.
    pub(super) fn handed_on(site: Site, share: usize) -> usize {
        COUNTS.with(|counts| {
            let mut seen = counts.get();
            seen[site as usize] += 1;
            counts.set(seen);
        });
        if DROP_PASS_ON.with(Cell::get) {
            0
        } else {
            share
        }
    }

    /// Reads and resets the counts, in the order of [`Site`].
    pub(in crate::sync) fn take() -> [usize; 3] {
        COUNTS.with(|counts| counts.replace([0; 3]))
    }

    /// Turns the calibration's mutation on or off.
    pub(in crate::sync) fn drop_pass_on(on: bool) {
        DROP_PASS_ON.with(|c| c.set(on));
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use core::future::Future;
    use core::pin::Pin;
    use core::task::Context;

    use super::*;
    use crate::sync::test_support::Polled;

    /// A wait for permits as a future, to poll by hand. Its output owns the
    /// permits, which the test gives back with `release`.
    struct Acquire<'a> {
        sem: &'a FairSemaphore,
        n: usize,
        wait: FairWait,
    }

    impl Future for Acquire<'_> {
        type Output = Result<(), Closed>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            this.wait.poll(this.sem, this.n, cx.waker())
        }
    }

    impl Drop for Acquire<'_> {
        fn drop(&mut self) {
            self.wait.cancel(self.sem, self.n);
        }
    }

    impl FairSemaphore {
        fn acquire(&self, n: usize) -> Acquire<'_> {
            Acquire {
                sem: self,
                n,
                wait: FairWait::new(),
            }
        }
    }

    /// The free permits, the set-aside and what the test holds add up to
    /// the semaphore's permits.
    fn set_aside(sem: &FairSemaphore) -> usize {
        // SAFETY: the tests call this with no pass running.
        sem.waiting.with(|waiting| unsafe { (*waiting).set_aside })
    }

    #[test]
    fn an_attempt_takes_what_is_free_while_nobody_waits() {
        let sem = FairSemaphore::new(3);
        assert!(sem.try_acquire(2).is_ok());
        assert_eq!(sem.available(), 1);
        assert!(sem.try_acquire(2).is_err());
        assert!(
            sem.try_acquire(0).is_ok(),
            "nothing asked, nothing to wait for"
        );
        sem.release(2);
        assert_eq!(sem.available(), 3);
    }

    #[test]
    fn waiters_are_granted_in_the_order_they_queued() {
        let sem = FairSemaphore::new(1);
        assert!(sem.try_acquire(1).is_ok());
        let mut first = Polled::new(sem.acquire(1));
        let mut second = Polled::new(sem.acquire(1));
        first.pending();
        second.pending();
        sem.release(1);
        assert_eq!((first.wakes(), second.wakes()), (1, 0));
        assert_eq!(first.ready(), Ok(()));
        second.pending();
        sem.release(1);
        assert_eq!(second.wakes(), 1);
        assert_eq!(second.ready(), Ok(()));
        sem.release(1);
        assert_eq!(sem.available(), 1);
    }

    #[test]
    fn a_later_small_request_does_not_pass_an_earlier_large_one() {
        let sem = FairSemaphore::new(3);
        assert!(sem.try_acquire(3).is_ok());
        let mut large = Polled::new(sem.acquire(3));
        large.pending();
        let mut small = Polled::new(sem.acquire(1));
        small.pending();
        sem.release(1);
        assert_eq!(
            sem.available(),
            0,
            "the free permit is set aside for the front"
        );
        assert_eq!(set_aside(&sem), 1);
        assert!(
            sem.try_acquire(1).is_err(),
            "an attempt does not pass a waiter"
        );
        assert_eq!(small.wakes(), 0, "the small request waits behind the large");
        sem.release(2);
        assert_eq!(large.wakes(), 1);
        assert_eq!(large.ready(), Ok(()));
        assert_eq!(small.wakes(), 0);
        sem.release(3);
        assert_eq!(small.ready(), Ok(()));
        assert_eq!(sem.available(), 2);
        sem.release(1);
        assert_eq!(sem.available(), 3);
    }

    #[test]
    fn a_front_waiter_that_leaves_passes_its_share_to_the_next() {
        let sem = FairSemaphore::new(3);
        assert!(sem.try_acquire(3).is_ok());
        let mut large = Polled::new(sem.acquire(3));
        large.pending();
        let mut next = Polled::new(sem.acquire(2));
        next.pending();
        sem.release(2);
        assert_eq!(set_aside(&sem), 2);
        drop(large);
        assert_eq!(next.wakes(), 1, "the share went on to the next waiter");
        assert_eq!(next.ready(), Ok(()));
        assert_eq!(set_aside(&sem), 0);
        assert_eq!(sem.available(), 0);
        sem.release(3);
        assert_eq!(sem.available(), 3);
    }

    #[test]
    fn a_waiter_granted_then_dropped_gives_its_permits_on() {
        let sem = FairSemaphore::new(1);
        assert!(sem.try_acquire(1).is_ok());
        let mut first = Polled::new(sem.acquire(1));
        let mut second = Polled::new(sem.acquire(1));
        first.pending();
        second.pending();
        sem.release(1);
        assert_eq!(first.wakes(), 1);
        drop(first);
        assert_eq!(second.wakes(), 1);
        assert_eq!(second.ready(), Ok(()));
        sem.release(1);
        assert_eq!(sem.available(), 1);
    }

    #[test]
    fn the_last_waiter_leaving_returns_the_set_aside() {
        let sem = FairSemaphore::new(4);
        assert!(sem.try_acquire(4).is_ok());
        let mut waiting = Polled::new(sem.acquire(4));
        waiting.pending();
        sem.release(3);
        assert_eq!(sem.available(), 0);
        drop(waiting);
        assert_eq!(sem.available(), 3);
        assert_eq!(set_aside(&sem), 0);
        assert!(sem.try_acquire(3).is_ok(), "nobody waits any more");
        sem.release(4);
    }

    #[test]
    fn closing_ends_every_wait_and_returns_the_set_aside() {
        let sem = FairSemaphore::new(2);
        assert!(sem.try_acquire(2).is_ok());
        let mut large = Polled::new(sem.acquire(2));
        let mut small = Polled::new(sem.acquire(1));
        large.pending();
        small.pending();
        sem.release(1);
        assert_eq!(sem.available(), 0);
        sem.close();
        assert_eq!(large.ready(), Err(Closed));
        assert_eq!(small.ready(), Err(Closed));
        assert_eq!(sem.available(), 1, "the set-aside came back");
        assert!(observed_closed(sem.try_acquire(1).unwrap_err()));
        assert!(matches!(Polled::new(sem.acquire(1)).ready(), Err(Closed)));
        sem.release(1);
        assert_eq!(sem.available(), 2);
    }

    #[test]
    fn many_cancelled_waits_leave_the_count_whole() {
        let sem = FairSemaphore::new(4);
        assert!(sem.try_acquire(4).is_ok());
        for round in 0..200 {
            let mut waits: Vec<_> = (0..5)
                .map(|i| Polled::new(sem.acquire(1 + (i + round) % 4)))
                .collect();
            for wait in &mut waits {
                wait.pending();
            }
            sem.release(1);
            // Drop them front first, middle first or back first in turn.
            match round % 3 {
                0 => drop(waits),
                1 => {
                    let back = waits.split_off(2);
                    drop(back);
                    drop(waits);
                }
                _ => {
                    waits.reverse();
                    drop(waits);
                }
            }
            assert_eq!(sem.available(), 1, "round {round}");
            assert_eq!(set_aside(&sem), 0, "round {round}");
            assert!(sem.try_acquire(1).is_ok(), "round {round}: nobody waits");
        }
        assert_eq!(sem.linked(), 0, "every cancelled node was let go");
        sem.release(4);
        assert_eq!(sem.available(), 4);
    }
}

#[cfg(all(test, not(loom)))]
impl FairSemaphore {
    /// Nodes on the waiting list, withdrawn ones included.
    fn linked(&self) -> usize {
        // SAFETY: tests call this with no pass running.
        self.waiting
            .with(|waiting| unsafe { (*waiting).list.len() })
    }
}
