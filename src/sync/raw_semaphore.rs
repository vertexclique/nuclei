//! The permit core of [`Semaphore`](super::Semaphore), which
//! [`Mutex`](super::Mutex), [`ReentrantMutex`](super::ReentrantMutex) and
//! [`OnceCell`](super::OnceCell) build on too. The bounded channel serves its
//! slots strictly in order instead, through `fair_semaphore`, on the same
//! state word layout.
//!
//! The state word holds the free permits above the queue's bits and one
//! closed bit. A request takes its permits whenever enough are free and no
//! handoff is owed, past any waiters (see `contend`). A release nudges the
//! oldest waiters the freed permits could serve, oldest first and stopping
//! at the first that does not fit, so a large request at the front is not
//! skipped by the nudges for smaller ones behind it; while a handoff is
//! owed, it hands permits to the waiters in queue order instead.
//!
//! # Closing
//!
//! [`close`](RawSemaphore::close) raises the closed bit with a draining
//! transition, so the pass it ends in sees the bit. A closed semaphore
//! admits nobody, whatever permits it holds, and its pass grants every
//! waiter a `CLOSED_GRANT`: the waiter ends in `Closed` and has nothing to
//! give back. A waiter that queues after the bit went up is granted the same
//! way by the pass its own enqueue runs. Permits that come back, or are
//! added, still count: they are only not handed out.

#![allow(unsafe_code)]

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use super::contend::{self, CLOSED_GRANT, Closed, Contend, Lock};
use super::internal::{Ordering, UnsafeCell, fence};
use super::list::List;
use super::queue::{
    Arrivals, FIRST_BIT, HANDOFF, Nudges, Policy, QUEUED, Step, WAKE_FRONT, WaitQueue, wants_drain,
};
use super::waiter::WakeList;

/// The semaphore is closed: nothing is admitted and every waiter is granted
/// the error. The permits sit above it.
const CLOSED: usize = 1 << FIRST_BIT;
pub(super) const SHIFT: u32 = FIRST_BIT + 1;
const FLAGS: usize = (1 << SHIFT) - 1;
/// One permit, in the units of the state word.
pub(super) const ONE: usize = 1 << SHIFT;
/// The most permits a semaphore can hold: the state word keeps the queue's
/// bits and the closed bit below them.
pub(super) const MAX: usize = usize::MAX >> SHIFT;

/// Whether a state word says the semaphore closed.
pub(super) fn closed_in(state: usize) -> bool {
    state & CLOSED != 0
}

/// The transition that closes a semaphore's state word: always a draining
/// one, even with nobody queued, so a waiter that queues concurrently either
/// is seen by this pass or finds the bit in the pass its own enqueue runs.
/// Closing twice does nothing.
pub(super) fn close_step(state: usize) -> Step {
    if closed_in(state) {
        Step::Keep
    } else {
        Step::Drain(state | CLOSED)
    }
}

/// Whether a state word seen by a failed attempt says the semaphore closed.
///
/// An attempt reads the word without ordering, which is all it needs to take
/// a permit, but a caller told "closed" may go on to read what the closer
/// published before it closed (the once-cell's value). So a closed word is
/// followed by an acquire fence: it synchronizes with the closing
/// transition, and everything the closer did first is visible from here on.
pub(super) fn observed_closed(state: usize) -> bool {
    let closed = closed_in(state);
    if closed {
        fence(Ordering::Acquire);
    }
    closed
}

/// The permit core: a weighted semaphore with no value attached.
pub(super) struct RawSemaphore {
    queue: WaitQueue,
    waiting: UnsafeCell<List>,
}

// SAFETY: the waiting list is touched only by the drain role's holder.
unsafe impl Send for RawSemaphore {}
// SAFETY: as above.
unsafe impl Sync for RawSemaphore {}

impl RawSemaphore {
    loom_const_fn! {
        pub(super) fn new(permits: usize) -> Self {
            assert!(permits <= MAX, "a semaphore holds at most Semaphore::MAX_PERMITS permits");
            Self {
                queue: WaitQueue::new(permits << SHIFT),
                waiting: UnsafeCell::new(List::new()),
            }
        }
    }

    loom_const_fn! {
        /// A semaphore whose contended acquires never spin: for permits
        /// their holders keep across long waits of their own (a once-cell's
        /// initializer turn, for example), where a spin could
        /// only burn the caller's time.
        pub(super) fn without_spin(permits: usize) -> Self {
            assert!(permits <= MAX, "a semaphore holds at most Semaphore::MAX_PERMITS permits");
            Self {
                queue: WaitQueue::without_spin(permits << SHIFT),
                waiting: UnsafeCell::new(List::new()),
            }
        }
    }

    loom_const_fn! {
        /// A closed semaphore with no permits, whose contended acquires never
        /// spin: nothing waits on it.
        pub(super) fn new_closed() -> Self {
            Self {
                queue: WaitQueue::without_spin(CLOSED),
                waiting: UnsafeCell::new(List::new()),
            }
        }
    }

    pub(super) fn available(&self) -> usize {
        self.queue.state.load(Ordering::Acquire) >> SHIFT
    }

    /// Whether [`close`](Self::close) was called.
    pub(super) fn is_closed(&self) -> bool {
        closed_in(self.queue.state.load(Ordering::Acquire))
    }

    /// Takes `n` permits if they are free and no handoff is owed.
    pub(super) fn try_acquire(&self, n: usize) -> bool {
        self.try_take(n).is_ok()
    }

    /// [`try_acquire`](Self::try_acquire) that hands the permits back as a
    /// [`RawPermit`], and on failure the state word it saw, which says
    /// whether the semaphore closed (see [`observed_closed`]).
    pub(super) fn try_acquire_permit(&self, n: usize) -> Result<RawPermit<'_>, usize> {
        self.try_take(n).map(|()| RawPermit { sem: self, n })
    }

    /// One permit, attempted inline: a load of the word, then one
    /// compare-and-swap. When the load finds the free state of a
    /// [`Mutex`](super::Mutex) (one permit, every queue bit down) the swap
    /// expects that constant, so it does not wait on arithmetic over the
    /// loaded value; otherwise, when a permit is free and no handoff is
    /// owed, it swaps from exactly the state loaded to one permit fewer.
    /// Either way it is `Lock::try_take` for one permit made in place, with
    /// the window between the load and the swap as short as the caller's
    /// code allows. On failure, the state it saw, for the out-of-line path
    /// to start from.
    ///
    /// Measured with `benches/sync.rs`, against three other shapes:
    /// a swap from the loaded state only (2 cycles slower uncontended, the
    /// same contended); a swap that expects the free state only and leaves
    /// every other state to the out-of-line path (10 to 25% less throughput
    /// at 8 and 32 tasks, where a free lock with waiters queued is common
    /// and the longer window let the line go between the load and the swap);
    /// and a swap with no load first (7 to 9 cycles faster uncontended, as
    /// fast as `std::sync::Mutex`, but 20 to 30% less throughput at 8 and 32
    /// tasks: a swap bound to fail takes the line from the holder, and it
    /// fails whenever a queue bit is up, though the lock may be free). The
    /// load is what this costs uncontended: it cannot complete before the
    /// previous release's locked add.
    #[inline]
    pub(super) fn take_one(&self) -> Result<(), usize> {
        let state = &self.queue.state;
        let seen = state.load(Ordering::Relaxed);
        if seen == ONE {
            return state
                .compare_exchange(ONE, 0, Ordering::Acquire, Ordering::Relaxed)
                .map(drop);
        }
        if seen >= ONE && seen & (HANDOFF | CLOSED) == 0 {
            state
                .compare_exchange(seen, seen - ONE, Ordering::Acquire, Ordering::Relaxed)
                .map(drop)
        } else {
            Err(seen)
        }
    }

    /// One permit, from a state the caller saw: what a Mutex's `try_lock`
    /// runs when [`take_one`](Self::take_one) failed, in case the state
    /// changed since.
    #[inline(never)]
    pub(super) fn try_acquire_one_from(&self, seen: usize) -> bool {
        contend::take_from(self, 1, false, seen).is_ok()
    }

    /// Returns `n` permits taken earlier, nudging waiters if any.
    ///
    /// One `fetch_add` when nobody waits, or when a nudge is already on
    /// its way. It cannot overflow: the permits were in the semaphore
    /// before. A waiter queueing concurrently either was already flagged,
    /// and is seen by the drain here, or flags itself after this add and
    /// finds the permits in its own pass.
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

    /// Adds `n` new permits, nudging waiters if any.
    ///
    /// # Panics
    ///
    /// When the semaphore would hold more than
    /// [`Semaphore::MAX_PERMITS`](super::Semaphore::MAX_PERMITS).
    pub(super) fn add(&self, n: usize) {
        if n == 0 {
            return;
        }
        self.transition(|state| {
            let permits = match (state >> SHIFT).checked_add(n) {
                Some(permits) if permits <= MAX => permits,
                _ => panic!("a semaphore holds at most Semaphore::MAX_PERMITS permits"),
            };
            let next = (state & FLAGS) | (permits << SHIFT);
            if state & QUEUED != 0 {
                Step::Wake(next | WAKE_FRONT)
            } else {
                Step::Set(next)
            }
        });
    }

    /// Takes up to `n` free permits out of the semaphore for good and
    /// returns how many it took. Waiters are not told: what they could take
    /// is only less.
    pub(super) fn forget(&self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let state = &self.queue.state;
        let mut current = state.load(Ordering::Acquire);
        loop {
            let free = current >> SHIFT;
            let taken = free.min(n);
            match state.compare_exchange_weak(
                current,
                current - (taken << SHIFT),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return taken,
                Err(actual) => current = actual,
            }
        }
    }

    /// Closes the semaphore: every waiter ends in `Closed`, and nothing is
    /// admitted from now on. Closing twice does nothing.
    pub(super) fn close(&self) {
        self.transition(close_step);
    }
}

impl Lock for RawSemaphore {
    fn admit(&self, state: usize, need: usize) -> Option<usize> {
        (state >> SHIFT >= need && state & CLOSED == 0).then(|| state - (need << SHIFT))
    }

    fn give_back(&self, need: usize) {
        self.release(need);
    }

    fn closed(&self, state: usize) -> bool {
        observed_closed(state)
    }

    fn try_take(&self, need: usize) -> Result<(), usize> {
        // Nothing to take, so nothing to wait for, owed handoff or not; a
        // closed semaphore still refuses.
        if need == 0 {
            let seen = self.queue.state.load(Ordering::Relaxed);
            return if closed_in(seen) { Err(seen) } else { Ok(()) };
        }
        contend::take(self, need, false)
    }
}

impl Policy for RawSemaphore {
    fn queue(&self) -> &WaitQueue {
        &self.queue
    }

    fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, wake_front: bool) -> usize {
        let queue = &self.queue;
        // SAFETY: only the drain role's holder runs a pass.
        self.waiting.with_mut(|waiting| unsafe {
            let waiting = &mut *waiting;
            waiting.absorb(arrivals, queue);
            waiting.tidy(queue);
            if self.is_closed() {
                while let Some(node) = waiting.front(queue) {
                    waiting.pop_front();
                    node.as_ref().set_payload(CLOSED_GRANT);
                    woken.grant(node);
                }
                return QUEUED;
            }
            if queue.state.load(Ordering::Acquire) & HANDOFF != 0 {
                while let Some(node) = waiting.front(queue) {
                    let need = node.as_ref().payload();
                    if !self.take_for_waiter(need) {
                        break;
                    }
                    waiting.pop_front();
                    if !woken.grant(node) {
                        queue.state.fetch_add(need << SHIFT, Ordering::AcqRel);
                    }
                }
            }
            // While nobody is owed, the queue competes: wake the waiters
            // the free permits could serve, oldest first. After a release,
            // wake the front whatever it asks for, so a request the free
            // permits cannot cover still tries, loses, counts the bypass,
            // and is owed a handoff in the end rather than starving behind
            // smaller callers.
            if !queue.hands_off() && waiting.front(queue).is_some() {
                let mut budget = queue.state.load(Ordering::Acquire) >> SHIFT;
                let mut nudges = Nudges::new(woken, queue);
                for (at, node) in waiting.iter().enumerate() {
                    let need = node.as_ref().payload();
                    if need > budget && !(at == 0 && wake_front) {
                        break;
                    }
                    nudges.nudge(node);
                    budget = budget.saturating_sub(need);
                }
                nudges.finish();
            }
            if waiting.front(queue).is_none() {
                QUEUED
            } else {
                0
            }
        })
    }
}

impl Drop for RawSemaphore {
    fn drop(&mut self) {
        let queue = &self.queue;
        // SAFETY: `&mut self` rules out a concurrent pass.
        self.waiting
            .with_mut(|waiting| unsafe { (*waiting).clear(queue) });
    }
}

#[cfg(all(test, not(loom)))]
impl RawSemaphore {
    /// Nodes on the waiting list, withdrawn ones included.
    pub(super) fn linked(&self) -> usize {
        // SAFETY: tests call this with no pass running.
        self.waiting.with(|waiting| unsafe { (*waiting).len() })
    }
}

/// A future's wait for permits on a [`RawSemaphore`].
pub(super) struct PermitWait {
    contend: Contend,
}

impl PermitWait {
    pub(super) const fn new() -> Self {
        Self {
            contend: Contend::new(),
        }
    }

    #[inline]
    pub(super) fn is_queued(&self) -> bool {
        self.contend.is_queued()
    }

    pub(super) fn poll(&mut self, sem: &RawSemaphore, n: usize, waker: &Waker) -> Poll<()> {
        self.contend.poll(sem, n, waker)
    }

    /// [`poll`](Self::poll) for a semaphore that can close: ready with
    /// [`Closed`] when it closed before the wait took its permits.
    pub(super) fn poll_closable(
        &mut self,
        sem: &RawSemaphore,
        n: usize,
        waker: &Waker,
    ) -> Poll<Result<(), Closed>> {
        self.contend.poll_closable(sem, n, waker)
    }

    /// The wait of a one-permit semaphore (a Mutex). A first poll makes
    /// its attempt inline with [`RawSemaphore::take_one`]; if that fails, or
    /// this future is already queued, the rest goes out of line, starting
    /// from the state the inline attempt saw.
    #[inline]
    pub(super) fn poll_one(&mut self, sem: &RawSemaphore, waker: &Waker) -> Poll<()> {
        let seen = if self.contend.is_queued() {
            None
        } else {
            match sem.take_one() {
                Ok(()) => return Poll::Ready(()),
                Err(seen) => Some(seen),
            }
        };
        self.poll_contended(sem, waker, seen)
    }

    /// Out of line, but not cold: under contention this is every acquire.
    #[inline(never)]
    fn poll_contended(
        &mut self,
        sem: &RawSemaphore,
        waker: &Waker,
        seen: Option<usize>,
    ) -> Poll<()> {
        self.contend.poll_from(sem, 1, waker, seen)
    }

    /// Withdraws a pending wait, passing on permits handed over meanwhile.
    /// A future that never queued, or already left the queue, has nothing
    /// to withdraw: that test is inline, the withdrawal out of line.
    #[inline]
    pub(super) fn cancel(&mut self, sem: &RawSemaphore, n: usize) {
        if self.contend.is_queued() {
            self.cancel_queued(sem, n);
        }
    }

    #[inline(never)]
    fn cancel_queued(&mut self, sem: &RawSemaphore, n: usize) {
        self.contend.cancel(sem, n);
    }
}

/// Permits held on a [`RawSemaphore`], returned on drop.
pub(super) struct RawPermit<'a> {
    sem: &'a RawSemaphore,
    n: usize,
}

impl RawPermit<'_> {
    /// Keeps the permits out of the semaphore for good.
    pub(super) fn forget(mut self) {
        self.n = 0;
    }
}

impl Drop for RawPermit<'_> {
    fn drop(&mut self) {
        self.sem.release(self.n);
    }
}

/// The future [`RawSemaphore::acquire`] returns.
pub(super) struct RawAcquire<'a> {
    sem: &'a RawSemaphore,
    n: usize,
    wait: PermitWait,
}

impl RawSemaphore {
    /// Waits for `n` permits.
    pub(super) fn acquire(&self, n: usize) -> RawAcquire<'_> {
        RawAcquire {
            sem: self,
            n,
            wait: PermitWait::new(),
        }
    }
}

impl<'a> Future for RawAcquire<'a> {
    type Output = Result<RawPermit<'a>, Closed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.wait
            .poll_closable(this.sem, this.n, cx.waker())
            .map(|taken| {
                taken.map(|()| RawPermit {
                    sem: this.sem,
                    n: this.n,
                })
            })
    }
}

impl Drop for RawAcquire<'_> {
    fn drop(&mut self) {
        self.wait.cancel(self.sem, self.n);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// Every state word of a semaphore with up to three permits: each
    /// permit count with each combination of the queue's bits, which are
    /// all the bits below `SHIFT`.
    fn every_state() -> impl Iterator<Item = usize> {
        (0..=3).flat_map(|permits| (0..=FLAGS).map(move |flags| (permits << SHIFT) | flags))
    }

    /// The Mutex's inline first attempt is `try_take` for one permit made
    /// in place: from every state it succeeds exactly when `try_take`
    /// would, past queued waiters too but never past an owed handoff, and
    /// leaves the word `try_take` leaves. On failure it reports the state
    /// it saw, for the out-of-line path to start from.
    #[test]
    fn the_inline_attempt_takes_exactly_what_try_take_takes() {
        for state in every_state() {
            let inline = RawSemaphore::new(0);
            inline.queue.state.store(state, Ordering::Relaxed);
            let full = RawSemaphore::new(0);
            full.queue.state.store(state, Ordering::Relaxed);
            let took = inline.take_one();
            assert_eq!(
                took.is_ok(),
                full.try_take(1).is_ok(),
                "state {state:#x}: the inline attempt and try_take disagree"
            );
            assert_eq!(
                inline.queue.state.load(Ordering::Relaxed),
                full.queue.state.load(Ordering::Relaxed),
                "state {state:#x}: the two leave different words"
            );
            if let Err(seen) = took {
                assert_eq!(seen, state, "a failed attempt reports the state it saw");
            }
            assert_eq!(
                took.is_ok(),
                state >> SHIFT >= 1 && state & (HANDOFF | CLOSED) == 0,
                "state {state:#x}: a free permit is taken unless a handoff is owed or it is closed"
            );
        }
    }
}
