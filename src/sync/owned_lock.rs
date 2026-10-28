//! An exclusive lock whose holder is named in its state word: a lock held
//! across separate calls by an owner, with no guard.
//!
//! The holder is an [`OwnerId`], the address of something aligned past the
//! wait queue's bits, so it rides in the state word above them. Taking the
//! lock writes the holder's name in the same compare-and-swap that takes
//! it, so a second call of the same holder, on any thread, reads its own
//! name back and knows it holds the lock; and an unlock names its holder,
//! so a stale or repeated unlock changes nothing.
//!
//! Waiting is the other locks' waiting (`contend`, `queue`, `waiter`):
//! barging with bounded bypass, a handoff that names the waiter's
//! owner in the state word, a cancellation that passes a handoff on, and no
//! lost wakeup.
//!
//! A handoff is two steps: the pass writes the waiter's name, then grants the
//! waiter, and a waiter that withdrew in between is not granted and the name
//! is cleared again. So the name a handoff writes carries [`GRANTING`] until
//! the grant is through, and while it does it is nobody's name: neither the
//! waiter's own poll nor another call of its owner can take it for a hold
//! (a poll that took it for one would run on a lock it never held). What a
//! waiter is woken through is its caller's `Waker`.

#![allow(unsafe_code)]

use core::task::{Poll, Waker};

use super::contend::{Contend, Lock};
use super::internal::{AtomicUsize, Ordering, UnsafeCell};
use super::list::List;
use super::queue::{
    Arrivals, FIRST_BIT, HANDOFF, Nudges, Policy, QUEUED, Step, WaitQueue, wants_drain,
};
use super::waiter::WakeList;

#[cfg(test)]
std::thread_local! {
    /// Run once by the next handoff on this thread, after it wrote the
    /// waiter's name and before it granted the waiter: what another thread
    /// could do between the two.
    static BETWEEN_TAKE_AND_GRANT: core::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { core::cell::Cell::new(None) };
}

/// Have the next handoff on this thread run `hook` between writing the
/// waiter's name and granting it.
#[cfg(test)]
pub(crate) fn between_take_and_grant(hook: impl FnOnce() + 'static) {
    BETWEEN_TAKE_AND_GRANT.set(Some(Box::new(hook)));
}

/// The queue's bits; the holder's name is everything above them.
const FLAGS: usize = (1 << FIRST_BIT) - 1;

/// Beside a name a handoff wrote and has not granted yet: the lowest name bit,
/// which [`OwnerId::ALIGN`] keeps clear in every name.
const GRANTING: usize = 1 << FIRST_BIT;

/// The name of a lock holder: a nonzero address whose low bits are clear,
/// so it fits above the wait queue's bits in the state word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct OwnerId(usize);

impl OwnerId {
    /// The alignment a holder's address needs: past the queue's bits and
    /// [`GRANTING`].
    pub(crate) const ALIGN: usize = GRANTING << 1;

    /// The name of `owner`, whose address names it for as long as it lives.
    /// The caller keeps `owner` alive while the name is in any lock.
    pub(crate) fn of<T>(owner: &T) -> Self {
        const { assert!(core::mem::align_of::<T>() >= OwnerId::ALIGN) };
        Self(core::ptr::from_ref(owner).addr())
    }

    /// The name as the state word carries it.
    pub(crate) fn word(self) -> usize {
        self.0
    }
}

/// What one attempt to take an [`OwnedLock`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TryLock {
    /// The lock was free and is now the caller's.
    Taken,
    /// The caller's owner already holds it.
    Mine,
    /// Another owner holds it, or a handoff is owed to a waiter.
    Busy,
}

/// An exclusive lock held by an [`OwnerId`] rather than by a guard.
pub(crate) struct OwnedLock {
    queue: WaitQueue,
    waiting: UnsafeCell<List>,
    /// `1` when [`OwnedLock::wake_all`] asked for it: the next pass nudges
    /// every waiter, not only the front, so each one runs its call again and
    /// sees what changed.
    wake_all: AtomicUsize,
}

// SAFETY: the waiting list is touched only by the drain role's holder.
unsafe impl Send for OwnedLock {}
// SAFETY: as above; everything else is atomic.
unsafe impl Sync for OwnedLock {}

impl OwnedLock {
    /// A free lock.
    pub(crate) fn new() -> Self {
        Self {
            queue: WaitQueue::new(0),
            waiting: UnsafeCell::new(List::new()),
            wake_all: AtomicUsize::new(0),
        }
    }

    /// The holder's name as the state word carries it, `0` when free. While a
    /// handoff is granting, the name carries [`GRANTING`] and equals no
    /// owner's name.
    pub(crate) fn holder(&self) -> usize {
        self.queue.state.load(Ordering::Acquire) & !FLAGS
    }

    /// One attempt to take the lock for `owner`: it succeeds when the lock
    /// is free and no handoff is owed, even while others wait (barging).
    /// Never waits.
    pub(crate) fn try_lock(&self, owner: OwnerId) -> TryLock {
        match self.try_take(owner.word()) {
            Ok(()) => TryLock::Taken,
            Err(seen) if seen & !FLAGS == owner.word() => TryLock::Mine,
            Err(_) => TryLock::Busy,
        }
    }

    /// Whether the state word carries `owner`'s name, a handoff's marked one
    /// included: what a poll must not take for a hold.
    #[cfg(all(loom, test))]
    pub(crate) fn names(&self, owner: OwnerId) -> bool {
        self.holder() & !GRANTING == owner.word()
    }

    /// Let go of the lock if `owner` holds it, waking the waiters a release
    /// should. `false`, changing nothing, when `owner` does not hold it.
    ///
    /// A waiter granted the lock may let it go before the pass that granted
    /// it lifts [`GRANTING`], so the marked name is the owner's here too: only
    /// the granted waiter, or its cancel giving the grant back, unlocks it.
    pub(crate) fn unlock(&self, owner: OwnerId) -> bool {
        let state = &self.queue.state;
        let mut current = state.load(Ordering::Acquire);
        loop {
            if current & !FLAGS & !GRANTING != owner.word() {
                return false;
            }
            match state.compare_exchange_weak(
                current,
                current & FLAGS,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if wants_drain(current) {
                        self.released();
                    }
                    return true;
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Wake every waiter now, so each runs its call again and sees what
    /// changed.
    /// Grants and nudges go on as before afterwards.
    pub(crate) fn wake_all(&self) {
        self.wake_all.store(1, Ordering::Release);
        self.transition(Step::Drain);
    }

    /// Lift [`GRANTING`] from the name a pass wrote for `owner`, once the
    /// grant is through. Nothing when the granted waiter already let go.
    fn granted(&self, owner: usize) {
        let state = &self.queue.state;
        let mut current = state.load(Ordering::Acquire);
        while current & !FLAGS == owner | GRANTING {
            match state.compare_exchange_weak(
                current,
                current & !GRANTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Clear the holder a pass just named for a waiter that had withdrawn.
    /// Only the drainer calls it, on a name it wrote itself.
    fn clear_holder(&self, owner: usize) {
        let state = &self.queue.state;
        let mut current = state.load(Ordering::Acquire);
        while current & !FLAGS == owner {
            match state.compare_exchange_weak(
                current,
                current & FLAGS,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Nodes on the waiting list, withdrawn ones included.
    #[cfg(all(test, not(loom)))]
    pub(crate) fn linked(&self) -> usize {
        // SAFETY: tests call this with no pass running.
        self.waiting.with(|waiting| unsafe { (*waiting).len() })
    }
}

impl Default for OwnedLock {
    fn default() -> Self {
        Self::new()
    }
}

impl Lock for OwnedLock {
    /// An owner keeps this lock across calls of its own, long waits among
    /// them, so a holder is seldom about to let go when another owner asks:
    /// a spin would only burn the asker's time. Its waiters queue at once.
    const SPINS: bool = false;

    /// A waiter's `need` is its owner's name: the lock is admitted to it
    /// only while free, and then names it.
    fn admit(&self, state: usize, need: usize) -> Option<usize> {
        (state & !FLAGS == 0).then_some(state | need)
    }

    fn give_back(&self, need: usize) {
        self.unlock(OwnerId(need));
    }
}

impl Policy for OwnedLock {
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
            if self.wake_all.swap(0, Ordering::AcqRel) != 0 {
                let mut nudges = Nudges::new(woken, queue);
                for node in waiting.iter() {
                    nudges.nudge(node);
                }
                nudges.finish();
            }
            // Whether this pass handed the lock over: the release that asked
            // for a wake was served by the handoff, so it wakes nobody else.
            let mut handed = false;
            if queue.state.load(Ordering::Acquire) & HANDOFF != 0 {
                // Owed: the front takes the free lock in its owner's name.
                // A front that withdrew meanwhile gives it straight back and
                // the next front is tried; one granted holds it, so the loop
                // ends there.
                while let Some(node) = waiting.front(queue) {
                    let owner = node.as_ref().payload();
                    if !self.take_for_waiter(owner | GRANTING) {
                        break;
                    }
                    #[cfg(test)]
                    if let Some(hook) = BETWEEN_TAKE_AND_GRANT.take() {
                        hook();
                    }
                    waiting.pop_front();
                    if woken.grant(node) {
                        self.granted(owner);
                        handed = true;
                    } else {
                        self.clear_holder(owner | GRANTING);
                    }
                }
            }
            // While nobody is owed, the front competes: woken when the lock
            // is free, or after a release whatever the lock's state, so a
            // waiter beaten to it counts the loss and is owed in the end.
            if !queue.hands_off()
                && let Some(front) = waiting.front(queue)
                && ((wake_front && !handed) || queue.state.load(Ordering::Acquire) & !FLAGS == 0)
            {
                let mut nudges = Nudges::new(woken, queue);
                nudges.nudge(front);
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

impl Drop for OwnedLock {
    fn drop(&mut self) {
        let queue = &self.queue;
        // SAFETY: `&mut self` rules out a concurrent pass.
        self.waiting
            .with_mut(|waiting| unsafe { (*waiting).clear(queue) });
    }
}

/// One owner's wait for an [`OwnedLock`]: its place in the queue, kept
/// across polls so the losses it counts stay its own.
pub(crate) struct OwnedWait {
    contend: Contend,
}

impl OwnedWait {
    pub(crate) const fn new() -> Self {
        Self {
            contend: Contend::new(),
        }
    }

    /// Whether the wait is in the lock's queue.
    pub(crate) fn is_queued(&self) -> bool {
        self.contend.is_queued()
    }

    /// Ready once `owner` holds the lock, by its own attempt or by a
    /// handoff. The first poll tries once and then queues; a later poll after
    /// a wake tries again, counting a loss toward the bypass bound. `waker`
    /// is woken when the lock may be the waiter's.
    ///
    /// The caller checks first that `owner` does not already hold the lock:
    /// a wait for a lock its own owner holds would never be served.
    pub(crate) fn poll(&mut self, lock: &OwnedLock, owner: OwnerId, waker: &Waker) -> Poll<()> {
        self.contend.poll(lock, owner.word(), waker)
    }

    /// Leave the queue. A handoff that reached the wait meanwhile is given
    /// back, so the lock goes to the next waiter or is freed.
    pub(crate) fn cancel(&mut self, lock: &OwnedLock, owner: OwnerId) {
        self.contend.cancel(lock, owner.word());
    }
}

impl Drop for OwnedWait {
    fn drop(&mut self) {
        // A queued wait holds a node only `cancel` can let go of, with the
        // lock in hand; dropping it instead would strand the node.
        debug_assert!(
            !self.is_queued(),
            "a queued lock wait was dropped without a cancel"
        );
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::super::MAX_BYPASS;
    use super::super::test_support::Polled;
    use super::*;

    /// An owner: aligned for its name, and one byte so each has its own
    /// address.
    #[repr(align(128))]
    struct Token(#[allow(dead_code)] u8);

    fn polled_wait<'a>(
        lock: &'a OwnedLock,
        owner: OwnerId,
        wait: &'a mut OwnedWait,
    ) -> Polled<impl core::future::Future<Output = ()> + 'a> {
        Polled::new(core::future::poll_fn(move |cx| {
            wait.poll(lock, owner, cx.waker())
        }))
    }

    #[test]
    fn the_holder_is_named_in_the_take_and_only_it_unlocks() {
        let (a, b) = (Token(0), Token(0));
        let (a, b) = (OwnerId::of(&a), OwnerId::of(&b));
        let lock = OwnedLock::new();
        assert_eq!(lock.holder(), 0);
        assert_eq!(lock.try_lock(a), TryLock::Taken);
        assert_eq!(lock.holder(), a.word());
        assert_eq!(lock.try_lock(a), TryLock::Mine, "a reads its own name");
        assert_eq!(lock.try_lock(b), TryLock::Busy);
        assert!(!lock.unlock(b), "b does not hold it");
        assert_eq!(lock.holder(), a.word());
        assert!(lock.unlock(a));
        assert!(!lock.unlock(a), "a second unlock changes nothing");
        assert_eq!(lock.try_lock(b), TryLock::Taken);
    }

    #[test]
    fn a_release_nudges_the_front_and_a_barger_still_wins() {
        let (a, b, c) = (Token(0), Token(0), Token(0));
        let (a, b, c) = (OwnerId::of(&a), OwnerId::of(&b), OwnerId::of(&c));
        let lock = OwnedLock::new();
        assert_eq!(lock.try_lock(a), TryLock::Taken);
        let mut wait = OwnedWait::new();
        let mut waiter = polled_wait(&lock, b, &mut wait);
        waiter.pending();
        assert!(lock.unlock(a));
        assert_eq!(waiter.wakes(), 1, "the release woke the front");
        assert_eq!(lock.try_lock(c), TryLock::Taken, "a free lock is barged");
        waiter.pending();
        assert!(lock.unlock(c));
        waiter.ready();
        assert_eq!(lock.holder(), b.word());
    }

    #[test]
    fn a_waiter_passed_over_max_bypass_times_is_handed_the_lock() {
        let (a, b) = (Token(0), Token(0));
        let (a, b) = (OwnerId::of(&a), OwnerId::of(&b));
        let lock = OwnedLock::new();
        assert_eq!(lock.try_lock(a), TryLock::Taken);
        let mut wait = OwnedWait::new();
        let mut waiter = polled_wait(&lock, b, &mut wait);
        waiter.pending();
        for round in 1..MAX_BYPASS {
            assert!(lock.unlock(a));
            assert_eq!(lock.try_lock(a), TryLock::Taken, "round {round}: barged");
            waiter.pending();
        }
        // The last loss: the waiter is owed, and the next unlock hands the
        // lock over in its name; nobody barges meanwhile.
        assert!(lock.unlock(a));
        assert_eq!(lock.try_lock(a), TryLock::Taken);
        waiter.pending();
        assert!(lock.unlock(a));
        assert_eq!(lock.holder(), b.word(), "handed over in b's name");
        assert_eq!(lock.try_lock(a), TryLock::Busy, "nobody barges a handoff");
        waiter.ready();
        assert!(lock.unlock(b));
        assert_eq!(lock.try_lock(a), TryLock::Taken, "barging resumes");
    }

    #[test]
    fn a_handoff_to_a_cancelled_waiter_passes_on() {
        let (a, b, c) = (Token(0), Token(0), Token(0));
        let (a, b, c) = (OwnerId::of(&a), OwnerId::of(&b), OwnerId::of(&c));
        let lock = OwnedLock::new();
        assert_eq!(lock.try_lock(a), TryLock::Taken);
        let mut first = OwnedWait::new();
        let mut second = OwnedWait::new();
        {
            let mut one = polled_wait(&lock, b, &mut first);
            one.pending();
            let mut two = polled_wait(&lock, c, &mut second);
            two.pending();
        }
        // Owe the first waiter by making it lose until the bound.
        for _ in 0..MAX_BYPASS {
            assert!(lock.unlock(a));
            assert_eq!(lock.try_lock(a), TryLock::Taken);
            let mut one = polled_wait(&lock, b, &mut first);
            let _ = one.poll();
        }
        assert!(lock.unlock(a));
        assert_eq!(lock.holder(), b.word(), "handed to the owed waiter");
        // It is cancelled before it polls: the handoff is given back, and
        // the lock reaches the next waiter.
        first.cancel(&lock, b);
        let mut two = polled_wait(&lock, c, &mut second);
        two.ready();
        assert_eq!(lock.holder(), c.word());
        assert!(lock.unlock(c));
        assert_eq!(lock.holder(), 0);
    }

    #[test]
    fn wake_all_nudges_every_waiter() {
        let (a, b, c) = (Token(0), Token(0), Token(0));
        let (a, b, c) = (OwnerId::of(&a), OwnerId::of(&b), OwnerId::of(&c));
        let lock = OwnedLock::new();
        assert_eq!(lock.try_lock(a), TryLock::Taken);
        let mut first = OwnedWait::new();
        let mut second = OwnedWait::new();
        let mut one = polled_wait(&lock, b, &mut first);
        let mut two = polled_wait(&lock, c, &mut second);
        one.pending();
        two.pending();
        lock.wake_all();
        assert_eq!((one.wakes(), two.wakes()), (1, 1));
        one.pending();
        two.pending();
        drop((one, two));
        first.cancel(&lock, b);
        second.cancel(&lock, c);
        assert!(lock.unlock(a));
        assert_eq!(lock.try_lock(b), TryLock::Taken);
    }

    #[test]
    fn owners_on_threads_never_overlap() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const ITERATIONS: usize = if cfg!(miri) { 20 } else { 2_000 };
        let lock = Arc::new(OwnedLock::new());
        let inside = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (lock, inside, total) =
                    (Arc::clone(&lock), Arc::clone(&inside), Arc::clone(&total));
                std::thread::spawn(move || {
                    let token = Token(0);
                    let me = OwnerId::of(&token);
                    for _ in 0..ITERATIONS {
                        if lock.try_lock(me) != TryLock::Taken {
                            let mut wait = OwnedWait::new();
                            crate::sync::test_support::block_on(core::future::poll_fn(|cx| {
                                wait.poll(&lock, me, cx.waker())
                            }));
                        }
                        assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0, "two holders");
                        total.fetch_add(1, Ordering::SeqCst);
                        inside.fetch_sub(1, Ordering::SeqCst);
                        assert!(lock.unlock(me));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("worker");
        }
        assert_eq!(total.load(Ordering::SeqCst), 4 * ITERATIONS);
        assert_eq!(lock.holder(), 0);
        assert_eq!(lock.linked(), 0, "no node left behind");
    }
}
