//! The acquire every lock-like primitive shares: take what is free at
//! once, otherwise spin briefly, then wait in the queue with a bounded
//! bypass.
//!
//! A caller takes what it asks for whenever it is free and no handoff is
//! owed, even past queued waiters: the thread that has just released a
//! lock and asks again takes it straight back, with no context switch.
//! When that one attempt fails, the caller spins on the state word for up
//! to the lock's budget (see `spin`), taking what is freed meanwhile, and
//! stops at once when a handoff is owed or waiters sit queued with nobody
//! changing the queue; then it queues and returns `Pending`. A release
//! wakes the front waiter, whatever it asks for, and the waiters behind it
//! the free permits could serve; a woken waiter tries again when polled.
//! Each time it cannot take its whole request it counts one bypass, and
//! after a loss it spins on its own node before it sleeps, so a release
//! that comes meanwhile marks it without a wake. The attempt that would be
//! its [`MAX_BYPASS`]th loss is owed a handoff in the same
//! compare-and-swap that finds it short, so barging stops before anything
//! more is freed, and released permits gather for it until its request is
//! met. No clock is read: fairness is counted, not timed, and a spin is
//! counted in rounds.
//!
//! Why the spin looks seldom and stops behind a settled queue, measured
//! with `benches/sync.rs`: a spin that loaded the word on every
//! round pulled the line from the holder on every round, and two tasks ran
//! at under half the throughput they reach with loads spaced by doubling
//! gaps; a spin that went on past queued waiters doubled two tasks' gain
//! again but halved eight and thirty-two tasks' throughput, where every
//! newcomer then spun on the shared line instead of waiting on its own
//! node. Queue-at-once (the design before) measured within 6% of a short
//! spin at eight and thirty-two tasks, and two tasks ran at 40% of
//! `std::sync::Mutex` with it.

use core::task::{Poll, Waker};

use super::internal::Ordering;
use super::queue::{HANDOFF, Policy, stops_spin};
use super::spin::Gaps;
use super::waiter::{Cancel, Park, Wait};

/// How many times a waiter can be passed over before it is owed a
/// handoff: a woken waiter that loses its retry this many times is handed
/// the next release directly, and nobody barges past it meanwhile.
#[cfg(not(loom))]
pub const MAX_BYPASS: u32 = 16;
/// Small under loom, so a model with a handful of operations reaches the
/// handoff.
#[cfg(loom)]
pub const MAX_BYPASS: u32 = 2;

/// A primitive whose waits take something and give it back.
pub(super) trait Lock: Policy {
    /// Whether a contended acquire spins before it queues, and after a
    /// lost retry before it sleeps (see `spin`). A primitive whose holders
    /// keep it across long waits of their own opts out.
    const SPINS: bool = true;

    /// `state` with `need` taken, when `state` has room for it; the owed
    /// bit aside.
    fn admit(&self, state: usize, need: usize) -> Option<usize>;

    /// Gives back what a waiter held, handing it on.
    fn give_back(&self, need: usize);

    /// One attempt to take `need` the way a fresh caller would: whenever it
    /// is free, but never while a handoff is owed. On failure, the state it
    /// saw.
    fn try_take(&self, need: usize) -> Result<(), usize> {
        take(self, need, false)
    }

    /// Takes `need` for a handoff, which may pass the owed bit because it
    /// serves the queue.
    fn take_for_waiter(&self, need: usize) -> bool {
        take(self, need, true).is_ok()
    }

    /// A queued waiter's attempt that would be its last counted loss: it
    /// takes `need` as [`try_take`](Self::try_take) would, or in the same
    /// compare-and-swap is owed a handoff (see `Policy::owe_or_take`).
    fn take_or_owe(&self, wait: &Wait, need: usize) -> Result<(), usize> {
        self.owe_or_take(wait, |state| self.admit(state, need))
    }
}

/// One compare-and-swap loop taking `need` through [`Lock::admit`]; past
/// an owed handoff only for the drain that hands it out.
pub(super) fn take<L: Lock + ?Sized>(lock: &L, need: usize, for_waiter: bool) -> Result<(), usize> {
    let failure = if for_waiter {
        Ordering::Acquire
    } else {
        Ordering::Relaxed
    };
    take_from(lock, need, for_waiter, lock.queue().state.load(failure))
}

/// [`take`] from a state the caller has just read, saving the load: the
/// first compare-and-swap expects `current`, and a stale value only fails it
/// once.
pub(super) fn take_from<L: Lock + ?Sized>(
    lock: &L,
    need: usize,
    for_waiter: bool,
    mut current: usize,
) -> Result<(), usize> {
    let (success, failure) = if for_waiter {
        (Ordering::AcqRel, Ordering::Acquire)
    } else {
        (Ordering::Acquire, Ordering::Relaxed)
    };
    let state = &lock.queue().state;
    loop {
        let next = match lock.admit(current, need) {
            Some(next) if for_waiter || current & HANDOFF == 0 => next,
            _ => return Err(current),
        };
        match state.compare_exchange_weak(current, next, success, failure) {
            Ok(_) => return Ok(()),
            Err(actual) => current = actual,
        }
    }
}

/// The spin before queueing: read the state word in gaps that double from
/// one pause (see `spin::Gaps`), for up to the lock's budget, and try the
/// compare-and-swap only when a read shows room. `seen` is the state the
/// caller's failed attempt saw. Stops at once when a handoff is owed, or
/// when waiters are queued and nobody holds the drain role
/// (`queue::stops_spin`): the caller then queues behind them, and the
/// budget is left as it was. A spin that runs out without ever seeing room
/// halves the budget; one that saw room but lost every swap keeps it. The
/// swap expects exactly the state it read, so it takes nothing a fresh
/// caller's attempt would not.
fn spin_take<L: Lock>(lock: &L, need: usize, mut seen: usize) -> bool {
    let queue = lock.queue();
    let rounds = queue.spin.rounds();
    if rounds == 0 {
        return false;
    }
    let mut gaps = Gaps::new(rounds);
    let mut freed = false;
    while gaps.wait() {
        if stops_spin(seen) {
            return false;
        }
        seen = queue.state.load(Ordering::Relaxed);
        if !stops_spin(seen)
            && let Some(next) = lock.admit(seen, need)
        {
            freed = true;
            match queue
                .state
                .compare_exchange(seen, next, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => {
                    queue.spin.won(rounds);
                    return true;
                }
                Err(actual) => seen = actual,
            }
        }
    }
    if !freed {
        queue.spin.ran_out(rounds);
    }
    false
}

/// A future's contended acquire.
pub(super) struct Contend {
    wait: Wait,
    bypass: u32,
}

impl Contend {
    pub(super) const fn new() -> Self {
        Self {
            wait: Wait::new(),
            bypass: 0,
        }
    }

    #[inline]
    pub(super) fn is_queued(&self) -> bool {
        self.wait.is_queued()
    }

    /// Ready once `need` is taken, by the caller's own attempt or by a
    /// handoff.
    pub(super) fn poll<L: Lock>(&mut self, lock: &L, need: usize, waker: &Waker) -> Poll<()> {
        self.poll_from(lock, need, waker, None)
    }

    /// [`poll`](Self::poll) for a caller that has just read the state word
    /// itself and found it `seen` (a Mutex's inline attempt): a first poll
    /// starts its attempt from that state instead of loading it again.
    pub(super) fn poll_from<L: Lock>(
        &mut self,
        lock: &L,
        need: usize,
        waker: &Waker,
        seen: Option<usize>,
    ) -> Poll<()> {
        let queue = lock.queue();
        let mut park = if self.wait.is_queued() {
            self.wait.park(queue, Some(waker))
        } else {
            // Losing this first attempt is not a bypass: nobody has passed
            // the caller yet, the holder was simply there first. Nor is
            // losing the spin that follows it, before the caller queues.
            let first = match seen {
                Some(seen) => take_from(lock, need, false, seen),
                None => lock.try_take(need),
            };
            match first {
                Ok(()) => return Poll::Ready(()),
                Err(seen) => {
                    if L::SPINS && spin_take(lock, need, seen) {
                        return Poll::Ready(());
                    }
                }
            }
            lock.enqueue(&mut self.wait, need, waker);
            self.wait.park(queue, None)
        };
        loop {
            match park {
                Park::Granted(_) => {
                    // A nudge may have been pending when the grant came.
                    queue.consume_nudge();
                    return Poll::Ready(());
                }
                Park::Pending => return Poll::Pending,
                // A nudge means something was freed after this waiter
                // queued, so losing the retry is a bypass even in the poll
                // that queued it: the nudge may come from this waiter's own
                // drain, and what others took during that drain has to be
                // counted like any other loss.
                Park::Nudged => {
                    queue.consume_nudge();
                    let attempt = if self.bypass + 1 == MAX_BYPASS {
                        lock.take_or_owe(&self.wait, need)
                    } else {
                        lock.try_take(need)
                    };
                    match attempt {
                        Ok(()) => {
                            // A retry that wins after a wake says holds can
                            // end soon after a release: a lock that stopped
                            // spinning tries again.
                            if L::SPINS && queue.spin.rounds() == 0 {
                                queue.spin.won(0);
                            }
                            self.cancel(lock, need);
                            return Poll::Ready(());
                        }
                        // Never past the bound: a waiter that is owed, was
                        // granted, and retries on a nudge left over from
                        // before loses once more to the permits taken for
                        // itself, which is no bypass.
                        Err(seen) if seen & HANDOFF == 0 => {
                            self.bypass = (self.bypass + 1).min(MAX_BYPASS);
                        }
                        Err(_) => {}
                    }
                    park = if L::SPINS {
                        self.wait.spin_park(queue, waker)
                    } else {
                        self.wait.park(queue, Some(waker))
                    };
                }
            }
        }
    }

    /// Leaves the queue: the future was dropped, or took what it waits
    /// for on its own. What a handoff gave it meanwhile is passed on.
    pub(super) fn cancel<L: Lock>(&mut self, lock: &L, need: usize) {
        match self.wait.cancel(lock.queue()) {
            Cancel::Idle => {}
            Cancel::Withdrawn(woken) => lock.withdrawn(woken),
            Cancel::Granted(_) => {
                lock.queue().consume_nudge();
                lock.give_back(need);
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use core::cell::Cell;

    use super::super::queue::{Arrivals, WaitQueue};
    use super::super::raw_semaphore::RawSemaphore;
    use super::super::test_support::Polled;
    use super::super::waiter::WakeList;
    use super::*;

    /// A semaphore whose waiter runs `after_loss` right after an attempt
    /// of its fails, before its poll goes on: what another thread could do
    /// between the two.
    struct Gap<'a> {
        sem: &'a RawSemaphore,
        after_loss: Cell<Option<Box<dyn FnOnce() + 'a>>>,
    }

    impl Gap<'_> {
        fn lost(&self, attempt: Result<(), usize>) -> Result<(), usize> {
            if attempt.is_err()
                && let Some(between) = self.after_loss.take()
            {
                between();
            }
            attempt
        }
    }

    impl Policy for Gap<'_> {
        fn queue(&self) -> &WaitQueue {
            self.sem.queue()
        }

        fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, wake_front: bool) -> usize {
            self.sem.pass(arrivals, woken, wake_front)
        }
    }

    impl Lock for Gap<'_> {
        fn admit(&self, state: usize, need: usize) -> Option<usize> {
            self.sem.admit(state, need)
        }

        fn give_back(&self, need: usize) {
            self.sem.give_back(need);
        }

        fn try_take(&self, need: usize) -> Result<(), usize> {
            self.lost(self.sem.try_take(need))
        }

        fn take_or_owe(&self, wait: &Wait, need: usize) -> Result<(), usize> {
            self.lost(self.sem.take_or_owe(wait, need))
        }
    }

    #[test]
    fn nothing_barges_between_a_waiters_last_loss_and_its_handoff() {
        let sem = RawSemaphore::new(1);
        let barged = Cell::new(false);
        let gap = Gap {
            sem: &sem,
            after_loss: Cell::new(None),
        };
        assert!(sem.try_acquire(1));
        let mut contend = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&gap, 1, cx.waker())
        }));
        waiter.pending();
        for round in 1..=MAX_BYPASS {
            sem.release(1);
            assert_eq!(waiter.wakes() as u32, round, "a release nudges it");
            assert!(sem.try_acquire(1), "a free permit is taken past it");
            if round < MAX_BYPASS {
                waiter.pending();
            }
        }
        // Its last loss: the holder lets go and someone asks at once,
        // before the waiter's poll returns.
        gap.after_loss.set(Some(Box::new(|| {
            sem.release(1);
            barged.set(sem.try_acquire(1));
        })));
        let served = waiter.poll().is_ready();
        assert!(
            !barged.get(),
            "a barger took the permit freed after the waiter's last counted loss"
        );
        assert!(served, "the freed permit is handed to the waiter");
        assert!(!sem.try_acquire(1), "the waiter holds it");
        sem.release(1);
        assert!(sem.try_acquire(1), "barging resumes once it is served");
    }

    /// A semaphore raced by other threads at chosen instants of one
    /// waiter's poll: as each pass of a drain that waiter runs begins and
    /// ends (each is told the pass's number, from 1), and just before its
    /// next attempt of each kind.
    struct Raced<'a> {
        sem: &'a RawSemaphore,
        passes: Cell<u32>,
        pass_begins: Box<dyn Fn(u32) + 'a>,
        pass_ends: Box<dyn Fn(u32) + 'a>,
        before_try: Cell<Option<Box<dyn FnOnce() + 'a>>>,
        before_owe: Cell<Option<Box<dyn FnOnce() + 'a>>>,
    }

    impl<'a> Raced<'a> {
        fn new(
            sem: &'a RawSemaphore,
            pass_begins: impl Fn(u32) + 'a,
            pass_ends: impl Fn(u32) + 'a,
        ) -> Self {
            Self {
                sem,
                passes: Cell::new(0),
                pass_begins: Box::new(pass_begins),
                pass_ends: Box::new(pass_ends),
                before_try: Cell::new(None),
                before_owe: Cell::new(None),
            }
        }
    }

    impl Policy for Raced<'_> {
        fn queue(&self) -> &WaitQueue {
            self.sem.queue()
        }

        fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, wake_front: bool) -> usize {
            let pass = self.passes.get() + 1;
            self.passes.set(pass);
            (self.pass_begins)(pass);
            let clear = self.sem.pass(arrivals, woken, wake_front);
            (self.pass_ends)(pass);
            clear
        }
    }

    impl Lock for Raced<'_> {
        fn admit(&self, state: usize, need: usize) -> Option<usize> {
            self.sem.admit(state, need)
        }

        fn give_back(&self, need: usize) {
            self.sem.give_back(need);
        }

        fn try_take(&self, need: usize) -> Result<(), usize> {
            if let Some(first) = self.before_try.take() {
                first();
            }
            self.sem.try_take(need)
        }

        fn take_or_owe(&self, wait: &Wait, need: usize) -> Result<(), usize> {
            if let Some(first) = self.before_owe.take() {
                first();
            }
            self.sem.take_or_owe(wait, need)
        }
    }

    /// A waiter that queues takes the drain role in the same poll.
    /// The holder lets go as its first pass begins, and from then on
    /// another caller takes the permit and gives it back after every pass,
    /// before the drain can give its role up. The drain must still end,
    /// and the waiter must get its turn.
    #[test]
    fn a_waiter_serving_the_queue_is_not_passed_over_without_bound() {
        let sem = RawSemaphore::new(1);
        let barged = Cell::new(0u32);
        let rounds = 4 * MAX_BYPASS;
        let raced = Raced::new(
            &sem,
            |pass| {
                if pass == 1 {
                    sem.release(1);
                }
            },
            |pass| {
                if pass <= rounds && sem.try_acquire(1) {
                    barged.set(barged.get() + 1);
                    sem.release(1);
                }
            },
        );
        assert!(sem.try_acquire(1));
        let mut contend = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&raced, 1, cx.waker())
        }));
        let served = waiter.poll().is_ready();
        assert!(
            barged.get() <= MAX_BYPASS,
            "{} callers took the permit past a queued waiter in {} passes of its own drain, \
             and it never got to retry (the bound is {MAX_BYPASS})",
            barged.get(),
            raced.passes.get(),
        );
        assert!(
            raced.passes.get() <= 3,
            "the waiter's drain ran {} passes: releases that come while its nudge is out \
             must not ask for more",
            raced.passes.get(),
        );
        assert!(
            served,
            "the permit the last caller gave back is the waiter's"
        );
        assert!(!sem.try_acquire(1), "the waiter holds it");
    }

    /// A nudge in the poll that queues a waiter means a permit was freed
    /// after it queued. Losing the retry to a caller that took that permit
    /// is a bypass like any other, also when the nudge and the loss both
    /// happen inside the waiter's own drain.
    #[test]
    fn a_retry_lost_in_the_poll_that_queues_counts_as_a_bypass() {
        let sem = RawSemaphore::new(1);
        let raced = Raced::new(
            &sem,
            |pass| {
                if pass == 1 {
                    sem.release(1);
                }
            },
            |pass| {
                if pass == 1 {
                    assert!(
                        sem.try_acquire(1),
                        "the freed permit is taken past the waiter"
                    );
                }
            },
        );
        assert!(sem.try_acquire(1));
        let mut contend = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&raced, 1, cx.waker())
        }));
        waiter.pending();
        drop(waiter);
        assert_eq!(
            contend.bypass, 1,
            "the loss in the queueing poll is counted"
        );
        contend.cancel(&raced, 1);
        sem.release(1);
        assert!(sem.try_acquire(1), "nothing is left held or owed");
    }

    /// A waiter that raised the owed bit, was handed the permit, and then
    /// retries on a nudge left over from before the handoff finds nothing
    /// free: the permit was taken for the waiter itself. That is not a
    /// bypass, and its count stays at the bound.
    #[test]
    fn a_loss_to_its_own_handoff_does_not_count_past_the_bound() {
        let sem = RawSemaphore::new(1);
        let raced = Raced::new(&sem, |_| {}, |_| {});
        assert!(sem.try_acquire(1));
        let mut contend = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&raced, 1, cx.waker())
        }));
        waiter.pending();
        // One loss short of the bound.
        for _ in 1..MAX_BYPASS {
            sem.release(1);
            assert!(sem.try_acquire(1), "a free permit is taken past it");
            waiter.pending();
        }
        sem.release(1);
        assert!(sem.try_acquire(1), "a free permit is taken past it");
        // Between the waiter's look at its node and its last counted
        // attempt, a release nudges it again and the permit is taken again:
        // the nudge stays on the node, unused.
        raced.before_owe.set(Some(Box::new(|| {
            sem.release(1);
            assert!(sem.try_acquire(1), "a free permit is taken past it");
        })));
        // It then retries on that nudge. Just before it does, the holder
        // lets go: the pass hands the permit to the owed waiter.
        raced.before_try.set(Some(Box::new(|| sem.release(1))));
        assert!(waiter.poll().is_ready(), "the handoff reached the waiter");
        drop(waiter);
        assert_eq!(
            contend.bypass, MAX_BYPASS,
            "a loss to the permit taken for the waiter itself was counted"
        );
        assert!(!sem.try_acquire(1), "the waiter holds it");
    }

    /// A thread that holds the drain role with nobody queued returns after
    /// its pass, however many releases other threads make meanwhile: a
    /// release that finds the role taken leaves only its request to wake
    /// the front, and with no front that request is dropped.
    #[test]
    fn a_drain_that_found_nobody_queued_does_not_pass_again_for_releases() {
        let sem = RawSemaphore::new(2);
        let hammering = Cell::new(false);
        let rounds = 4 * MAX_BYPASS;
        let raced = Raced::new(
            &sem,
            |_| {},
            |pass| {
                if hammering.get() && pass <= rounds {
                    // Another holder lets its permit go and takes it back,
                    // before the drain can give its role up.
                    sem.release(1);
                    assert!(sem.try_acquire(1), "the permit is free again");
                }
            },
        );
        assert!(sem.try_acquire(1), "the other holder's permit");
        let mut contend = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&raced, 2, cx.waker())
        }));
        waiter.pending();
        drop(waiter);
        // The waiter gives up. Its withdrawal takes the drain role, and the
        // pass finds nobody queued while the other holder hammers.
        let before = raced.passes.get();
        hammering.set(true);
        contend.cancel(&raced, 2);
        hammering.set(false);
        assert_eq!(
            raced.passes.get() - before,
            1,
            "the drain passed again for releases that had nobody to wake"
        );
        // Nothing is left behind: a waiter that queues now is not nudged for
        // a release that came before it, so it counts nothing. And the next
        // release reaches it.
        let mut next = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| next.poll(&raced, 2, cx.waker())));
        waiter.pending();
        sem.release(1);
        assert_eq!(waiter.wakes(), 1, "the release nudges the waiter");
        assert!(waiter.poll().is_ready(), "both permits are its");
        drop(waiter);
        assert_eq!(
            next.bypass, 0,
            "it retried on a request from before it queued"
        );
        sem.release(2);
        assert_eq!(sem.available(), 2, "a permit was lost or made up");
    }

    /// A release that comes while a pass runs must still reach the waiter
    /// that pass found, though it leaves no dirty bit: the drain passes
    /// again for its request.
    #[test]
    fn a_release_during_a_pass_that_found_a_waiter_reaches_it() {
        let sem = RawSemaphore::new(1);
        let raced = Raced::new(
            &sem,
            |_| {},
            |pass| {
                if pass == 1 {
                    sem.release(1);
                }
            },
        );
        assert!(sem.try_acquire(1));
        let mut contend = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&raced, 1, cx.waker())
        }));
        // The waiter queues and serves the queue; the holder lets go right
        // after its first pass read that nothing was free.
        assert!(
            waiter.poll().is_ready(),
            "a release that found the drain role taken was lost"
        );
        assert!(!sem.try_acquire(1), "the waiter holds it");
    }

    /// A semaphore watched by a contender: `looks` counts the times the
    /// contender judged a state word (each first attempt, retry and spin
    /// round does), and `at_look` runs, just after that judgement, what
    /// another thread does at that instant.
    struct Watched<'a> {
        sem: &'a RawSemaphore,
        looks: Cell<u32>,
        at_look: Box<dyn Fn(u32) + 'a>,
    }

    impl<'a> Watched<'a> {
        fn new(sem: &'a RawSemaphore, at_look: impl Fn(u32) + 'a) -> Self {
            Self {
                sem,
                looks: Cell::new(0),
                at_look: Box::new(at_look),
            }
        }
    }

    impl Policy for Watched<'_> {
        fn queue(&self) -> &WaitQueue {
            self.sem.queue()
        }

        fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, wake_front: bool) -> usize {
            self.sem.pass(arrivals, woken, wake_front)
        }
    }

    impl Lock for Watched<'_> {
        fn admit(&self, state: usize, need: usize) -> Option<usize> {
            let admitted = self.sem.admit(state, need);
            let look = self.looks.get() + 1;
            self.looks.set(look);
            (self.at_look)(look);
            admitted
        }

        fn give_back(&self, need: usize) {
            self.sem.give_back(need);
        }
    }

    /// B, the spin before queueing. The holder lets go while a contender
    /// spins (just after the contender's second look): the contender takes
    /// the lock in its first poll, never queues, is never woken, and the
    /// lock's budget doubles.
    #[test]
    fn a_contender_takes_a_lock_let_go_during_its_spin_without_queueing() {
        let sem = RawSemaphore::new(1);
        assert!(sem.try_acquire(1), "the holder takes it");
        let budget = sem.queue().spin.rounds();
        assert!(budget > 0, "a fresh lock spins");
        let watched = Watched::new(&sem, |look| {
            if look == 2 {
                sem.release(1);
            }
        });
        let mut contend = Contend::new();
        let mut contender = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&watched, 1, cx.waker())
        }));
        assert!(contender.poll().is_ready(), "the spin took the freed lock");
        assert_eq!(contender.wakes(), 0, "a spinner needs no wake");
        drop(contender);
        assert!(!contend.is_queued(), "the contender never queued");
        assert!(
            watched.looks.get() >= 3,
            "it looked again after the release"
        );
        assert_eq!(
            sem.queue().spin.rounds(),
            (2 * budget).min(u32::from(super::super::spin::MAX)),
            "a won spin doubles the budget"
        );
        assert!(!sem.try_acquire(1), "the contender holds it");
    }

    /// B. A spin that never sees room runs out, halves the budget, and the
    /// contender queues and is served by the release as before.
    #[test]
    fn a_spin_that_never_sees_room_halves_the_budget_and_queues() {
        let sem = RawSemaphore::new(1);
        assert!(sem.try_acquire(1));
        let budget = sem.queue().spin.rounds();
        let watched = Watched::new(&sem, |_| {});
        let mut contend = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&watched, 1, cx.waker())
        }));
        waiter.pending();
        assert!(watched.looks.get() > 1, "it spun before queueing");
        assert_eq!(
            sem.queue().spin.rounds(),
            budget / 2,
            "a spin that ran out halves"
        );
        sem.release(1);
        assert_eq!(waiter.wakes(), 1, "the release reaches the queued waiter");
        assert!(waiter.poll().is_ready());
    }

    /// B. A spin that saw the lock free but lost every swap to a barger keeps
    /// the budget: the holders are short, the lock is only busy.
    #[test]
    fn a_spin_that_saw_room_but_lost_keeps_the_budget() {
        let sem = RawSemaphore::new(1);
        assert!(sem.try_acquire(1));
        let budget = sem.queue().spin.rounds();
        let watched = Watched::new(&sem, |look| match look {
            // The holder lets go just after the contender's second look ...
            2 => sem.release(1),
            // ... and a barger takes the lock right after the contender saw
            // it free, before the contender's swap.
            3 => assert!(sem.try_acquire(1), "the barger takes it"),
            _ => {}
        });
        let mut contend = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&watched, 1, cx.waker())
        }));
        waiter.pending();
        assert_eq!(
            sem.queue().spin.rounds(),
            budget,
            "a busy lock keeps its budget"
        );
        drop(waiter);
        contend.cancel(&watched, 1);
        sem.release(1);
    }

    /// B. The spin stops at once behind a settled queue (waiters queued and
    /// nobody changing the queue): the contender looks once and queues, and
    /// the budget is left alone. The owed handoff, the spin's other stop, is
    /// pinned for every state word by
    /// `the_spin_takes_only_what_a_fresh_caller_may`.
    #[test]
    fn a_contender_queues_at_once_behind_a_settled_queue() {
        let sem = RawSemaphore::new(1);
        assert!(sem.try_acquire(1));
        let budget = sem.queue().spin.rounds();
        let plain = Watched::new(&sem, |_| {});
        let mut first = Contend::new();
        let mut first_waiter = Polled::new(core::future::poll_fn(|cx| {
            first.poll(&plain, 1, cx.waker())
        }));
        first_waiter.pending();
        let after_first = sem.queue().spin.rounds();
        let watched = Watched::new(&sem, |_| {});
        let mut second = Contend::new();
        let mut second_waiter = Polled::new(core::future::poll_fn(|cx| {
            second.poll(&watched, 1, cx.waker())
        }));
        second_waiter.pending();
        assert_eq!(watched.looks.get(), 1, "it looked once and did not spin");
        assert_eq!(
            sem.queue().spin.rounds(),
            after_first,
            "the budget is untouched"
        );
        assert!(after_first <= budget);
        drop((first_waiter, second_waiter));
        first.cancel(&plain, 1);
        second.cancel(&watched, 1);
        sem.release(1);
        assert!(sem.try_acquire(1), "nothing is left held or owed");
    }

    /// B, the spin's rule for every state word with up to two permits and
    /// every combination of the queue's six bits: one round takes a permit
    /// exactly when a fresh caller may (a permit free and no handoff owed)
    /// and the queue is not settled (waiters queued with nobody holding the
    /// drain role), and leaves the word as that take would; otherwise it
    /// changes nothing. A spin that ignored an owed handoff fails here at
    /// the word with one permit and HANDOFF up.
    #[test]
    fn the_spin_takes_only_what_a_fresh_caller_may() {
        const SHIFT: u32 = super::super::queue::FIRST_BIT;
        const DRAINING_BIT: usize = 1;
        const QUEUED_BIT: usize = 1 << 2;
        for permits in 0..=2usize {
            for flags in 0..(1usize << SHIFT) {
                let state = (permits << SHIFT) | flags;
                let sem = RawSemaphore::new(0);
                sem.queue().state.store(state, Ordering::Relaxed);
                let took = spin_take(&sem, 1, state);
                let settled = flags & (QUEUED_BIT | DRAINING_BIT) == QUEUED_BIT;
                let may = permits >= 1 && flags & HANDOFF == 0 && !settled;
                assert_eq!(took, may, "state {state:#x}: the spin took the wrong thing");
                let after = sem.queue().state.load(Ordering::Relaxed);
                let expected = if took { state - (1 << SHIFT) } else { state };
                assert_eq!(
                    after, expected,
                    "state {state:#x}: the spin left the wrong word"
                );
            }
        }
    }

    /// B. A queued waiter whose retry wins after a nudge brings a spent
    /// budget back to one, so a lock whose holds turned short spins again.
    #[test]
    fn a_won_retry_revives_a_spent_budget() {
        let sem = RawSemaphore::new(1);
        let spin = &sem.queue().spin;
        while spin.rounds() > 0 {
            spin.ran_out(spin.rounds());
        }
        assert!(sem.try_acquire(1));
        let mut contend = Contend::new();
        let mut waiter = Polled::new(core::future::poll_fn(|cx| {
            contend.poll(&sem, 1, cx.waker())
        }));
        waiter.pending();
        assert_eq!(spin.rounds(), 0, "a spent budget queues at once");
        sem.release(1);
        assert!(waiter.poll().is_ready(), "the retry wins");
        assert_eq!(spin.rounds(), 1, "the won retry revives the budget");
    }
}
