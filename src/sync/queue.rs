//! The wait queue and the drain role every primitive builds on.
//!
//! A [`WaitQueue`] is one state word, a lock-free stack of arriving waiter
//! nodes, the owed count, and the node pool. The five low bits of the
//! state word belong to the queue; a primitive keeps its own state in the
//! bits above them.
//!
//! # The drain role
//!
//! A drain pass is the only code that sorts arrivals, grants or nudges
//! waiters and prunes withdrawn ones, so it is single-consumer by
//! construction and its lists need no synchronization of their own. The
//! role is a bit in the state word, and nobody ever waits for it:
//!
//! - Every operation that changes who is queued (a new waiter, a
//!   withdrawal, a notification) ends in a [`Step::Drain`] transition: one
//!   compare-and-swap that applies the operation's own state change and
//!   either claims the role or, when another thread holds it, sets the
//!   dirty bit.
//! - A release ends in a [`Step::Wake`] transition: it claims the role the
//!   same way, but when another thread holds it, it leaves only its request
//!   ([`WAKE_FRONT`]) and no dirty bit. A release changes what a waiter
//!   could take, not who waits, so it matters to the holder only if the
//!   holder's pass found somebody queued.
//! - The holder clears the dirty bit and the request, then takes the
//!   arrival stack, runs its primitive's pass, and gives the role up with a
//!   compare-and-swap that requires the dirty bit still clear, and the
//!   request still clear unless the pass found nobody queued. Otherwise it
//!   passes again.
//!
//! So every change made while a pass runs is seen by that pass or by a
//! later one, and every change made after the role is given up claims it
//! afresh: no wakeup is lost, and no call waits on another thread.
//!
//! # What a drain costs its thread
//!
//! The drain is lock-free, not wait-free. The holder of the role runs one
//! pass. It runs another only if, since that pass began, a waiter queued,
//! withdrew or claimed the handoff it is owed (each sets the dirty bit),
//! or a release found a waiter queued (it leaves [`WAKE_FRONT`]). So a
//! drain runs at most one pass plus one for each such operation of another
//! thread.
//!
//! Releases alone never keep it at work. With nobody queued, a release
//! leaves no dirty bit and its request is void, so the drain does not pass
//! again for it. With somebody queued, the pass a release asks for nudges
//! the front waiter or finds it nudged already; either way [`NUDGE_OUT`]
//! stays up until that waiter looks, and the releases that follow skip the
//! drain altogether. While a handoff is owed every release asks for a
//! pass, but nobody can barge then, so only the holders there were can
//! release.
//!
//! Waiters that keep queueing or withdrawing can keep it at work, one pass
//! each: there is no cap on the passes of one call.
//!
//! Arrivals push onto a Treiber stack. Pushing is ABA-safe because nothing
//! pops single nodes: a pass takes the whole stack with one swap and
//! reverses it into arrival order.
//!
//! # Barging with bounded bypass
//!
//! The lock-like primitives let a caller take what is free even while
//! others wait, so a release never waits for a suspended task to run. A
//! release with waiters *nudges* the oldest ones it could serve: they stay
//! queued, are woken, and compete again when polled. [`NUDGE_OUT`] marks a
//! nudge whose waiter has not polled yet; releases skip the drain pass
//! while it is set, since a wake is already on its way. So a drain runs at
//! most one more pass for each release that found the marker down, and none
//! for the releases that come while a nudge is out.
//!
//! A nudged waiter that loses again counts one bypass. The attempt that
//! would be its [`MAX_BYPASS`](super::MAX_BYPASS)th loss marks it owed
//! first, then either takes what it asks for or raises the [`HANDOFF`]
//! bit in the same compare-and-swap. While the bit is up every barging
//! acquire is refused, so what a release frees stays parked in the state
//! word for the drain pass, which hands it to the waiters in queue order;
//! the bit clears once no owed waiter is left. No release can fall between
//! the last loss and the bit: one ordered before the swap is seen by it,
//! and the waiter takes what it freed. So a waiter is passed over at most
//! `MAX_BYPASS` times.

#![allow(unsafe_code)]

use core::ptr::{self, NonNull};

use super::internal::{AtomicPtr, AtomicUsize, Ordering};
use super::spin::Budget;
use super::waiter::{Pool, Wait, Waiter, WakeList, release_queued};

/// The drain role is held.
const DRAINING: usize = 1;
/// The state changed while the role was held, so its holder passes again.
const DIRTY: usize = 1 << 1;
/// Waiters may be queued.
pub(super) const QUEUED: usize = 1 << 2;
/// A nudge is on its way to a waiter that has not polled yet.
pub(super) const NUDGE_OUT: usize = 1 << 3;
/// An owed waiter is queued: nobody barges, releases hand off in order.
pub(super) const HANDOFF: usize = 1 << 4;
/// A release, or the withdrawal of a woken waiter, asks the next pass to
/// wake the front waiter whatever it asks for.
pub(super) const WAKE_FRONT: usize = 1 << 5;

/// The lowest state bit a primitive may use.
pub(super) const FIRST_BIT: u32 = 6;

/// Whether a release that saw `prev` must run a drain pass: waiters are
/// queued and no nudge is already on its way, or a handoff is owed.
#[inline]
pub(super) fn wants_drain(prev: usize) -> bool {
    prev & QUEUED != 0 && (prev & NUDGE_OUT == 0 || prev & HANDOFF != 0)
}

/// Whether a contender that saw `state` stops spinning and queues: a
/// handoff is owed, or waiters are queued and nobody is changing the queue.
/// A queue with the drain role taken may be emptying (a waiter that won its
/// retry is withdrawing), so a spinner looks again after its next gap.
#[inline]
pub(super) fn stops_spin(state: usize) -> bool {
    state & HANDOFF != 0 || state & (QUEUED | DRAINING) == QUEUED
}

/// The nudges of one pass. It raises [`NUDGE_OUT`] before the first nudge
/// leaves, so a waiter that polls at once still finds the marker to take
/// back, and lowers it again only if every nudge found its waiter gone.
///
/// A waiter that still carries an earlier nudge keeps the marker up. Were
/// it lowered then, every release would ask for a pass until that waiter
/// polls; and when that waiter is the thread running these passes (it
/// queued and took the drain role in the same poll), it cannot poll while
/// they keep coming, so it would be passed over for as long as releases
/// arrived. With the marker up those releases stay one read-modify-write,
/// the drain ends, and the waiter retries.
pub(super) struct Nudges<'p, 'a> {
    woken: &'p mut WakeList<'a>,
    queue: &'p WaitQueue,
    raised: bool,
    sent: bool,
}

impl<'p, 'a> Nudges<'p, 'a> {
    pub(super) fn new(woken: &'p mut WakeList<'a>, queue: &'p WaitQueue) -> Self {
        Self {
            woken,
            queue,
            raised: false,
            sent: false,
        }
    }

    pub(super) fn nudge(&mut self, node: NonNull<Waiter>) {
        if !self.raised {
            self.queue.state.fetch_or(NUDGE_OUT, Ordering::AcqRel);
            self.raised = true;
        }
        self.sent |= self.woken.nudge(node);
    }

    pub(super) fn finish(self) {
        if self.raised && !self.sent {
            self.queue.state.fetch_and(!NUDGE_OUT, Ordering::AcqRel);
        }
    }
}

/// What a [`Policy::transition`] does with the state it was shown.
pub(super) enum Step {
    /// Leave the state alone.
    Keep,
    /// Store this state.
    Set(usize),
    /// Store this state and make sure a drain pass sees it.
    Drain(usize),
    /// Store this state, which carries a release's [`WAKE_FRONT`] request,
    /// and make sure a pass acts on it if somebody is queued: claim the
    /// role, or leave the request with the thread that holds it, without
    /// the dirty bit.
    Wake(usize),
}

pub(super) struct WaitQueue {
    pub(super) state: AtomicUsize,
    arrivals: AtomicPtr<Waiter>,
    cancels: AtomicUsize,
    /// Queued waiters marked owed a handoff.
    owed: AtomicUsize,
    pool: Pool,
    /// How many rounds a contended acquire spins (see `spin`).
    pub(super) spin: Budget,
}

impl WaitQueue {
    loom_const_fn! {
        pub(super) fn new(state: usize) -> Self {
            Self::with_spin(state, Budget::new())
        }
    }

    loom_const_fn! {
        /// A queue whose contended acquires never spin (see `spin::Budget::never`).
        pub(super) fn without_spin(state: usize) -> Self {
            Self::with_spin(state, Budget::never())
        }
    }

    loom_const_fn! {
        fn with_spin(state: usize, spin: Budget) -> Self {
            Self {
                state: AtomicUsize::new(state),
                arrivals: AtomicPtr::new(ptr::null_mut()),
                cancels: AtomicUsize::new(0),
                owed: AtomicUsize::new(0),
                pool: Pool::new(),
                spin,
            }
        }
    }

    pub(super) fn pool(&self) -> &Pool {
        &self.pool
    }

    /// How many waiters have withdrawn, ever; lists compare it against
    /// their last sweep.
    pub(super) fn cancels(&self) -> usize {
        self.cancels.load(Ordering::Relaxed)
    }

    /// Whether the queue serves its waiters by handoff alone: the
    /// [`HANDOFF`] bit is up and a queued waiter is still owed. A pass then
    /// grants in queue order and wakes nobody to compete. The count alone
    /// is not enough: a waiter counts itself owed just before its last
    /// attempt, and may take what it asks for instead.
    pub(super) fn hands_off(&self) -> bool {
        self.state.load(Ordering::Acquire) & HANDOFF != 0 && self.owed.load(Ordering::Acquire) != 0
    }

    /// Settles one owed waiter, which the queue let go of.
    pub(super) fn forgive(&self) {
        self.owed.fetch_sub(1, Ordering::AcqRel);
    }

    /// Takes back the nudge marker as a nudged waiter polls; the state
    /// returned is current, so the caller re-checks against it.
    pub(super) fn consume_nudge(&self) -> usize {
        self.state.fetch_and(!NUDGE_OUT, Ordering::AcqRel)
    }

    pub(super) fn push(&self, node: NonNull<Waiter>) {
        let mut head = self.arrivals.load(Ordering::Relaxed);
        loop {
            // SAFETY: the caller owns `node` until this push publishes it.
            unsafe { node.as_ref() }.set_next(head);
            match self.arrivals.compare_exchange_weak(
                head,
                node.as_ptr(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => head = actual,
            }
        }
    }

    /// Takes every node pushed so far, oldest first.
    fn take(&self) -> Arrivals {
        let mut cursor = self.arrivals.swap(ptr::null_mut(), Ordering::Acquire);
        let mut fifo: *mut Waiter = ptr::null_mut();
        while let Some(node) = NonNull::new(cursor) {
            // SAFETY: the swap handed over every node on the stack.
            let waiter = unsafe { node.as_ref() };
            cursor = waiter.next();
            waiter.set_next(fifo);
            fifo = node.as_ptr();
        }
        Arrivals(fifo)
    }
}

impl Drop for WaitQueue {
    fn drop(&mut self) {
        // No future outlives its primitive, so whatever is still stacked
        // has withdrawn and only the queue's reference is left.
        for node in self.take() {
            release_queued(node, self);
        }
    }
}

/// Nodes taken off the arrival stack, oldest first, each with the queue's
/// reference. A pass must consume them all.
pub(super) struct Arrivals(*mut Waiter);

impl Iterator for Arrivals {
    type Item = NonNull<Waiter>;

    fn next(&mut self) -> Option<NonNull<Waiter>> {
        let node = NonNull::new(self.0)?;
        // SAFETY: the chain holds the queue's reference to every node.
        self.0 = unsafe { node.as_ref() }.next();
        Some(node)
    }
}

/// A primitive's half of the drain protocol.
pub(super) trait Policy {
    fn queue(&self) -> &WaitQueue;

    /// One pass with the drain role held: absorb `arrivals`, grant or
    /// nudge what the state allows into `woken`, and return the state bits
    /// to clear on the way out, which stand only if nothing changed
    /// meanwhile. `wake_front` says a release (or a woken waiter's
    /// withdrawal) asked for the front waiter to be woken whatever it asks
    /// for.
    ///
    /// [`QUEUED`] among the returned bits says the pass found nobody
    /// queued. The drain relies on it: a release's request that arrives
    /// while such a pass runs has no front waiter to wake and is dropped.
    fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, wake_front: bool) -> usize;

    /// Applies `f` to the state atomically and returns the state it last
    /// saw. A [`Step::Drain`] also claims the drain role, or marks its
    /// holder dirty; a [`Step::Wake`] claims it, or leaves its request with
    /// the holder; a claimed role is drained before this returns.
    fn transition(&self, mut f: impl FnMut(usize) -> Step) -> usize {
        let state = &self.queue().state;
        let mut current = state.load(Ordering::Acquire);
        loop {
            let (next, claim) = match f(current) {
                Step::Keep => return current,
                Step::Set(next) => (next, false),
                Step::Drain(next) if current & DRAINING != 0 => (next | DIRTY, false),
                Step::Wake(next) if current & DRAINING != 0 => (next, false),
                Step::Drain(next) | Step::Wake(next) => (next | DRAINING, true),
            };
            match state.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    if claim {
                        self.drain();
                    }
                    return current;
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Runs passes until one finishes with nothing new, then gives the
    /// role up and wakes what the passes granted or nudged. The caller
    /// holds the role.
    ///
    /// "Nothing new" is: no dirty bit, and no release's request for a pass
    /// that found somebody queued. A request that came while the pass found
    /// nobody queued is dropped with the role, so releases by other threads
    /// cannot keep this one passing over an empty queue. A waiter that
    /// queued after the pass took the arrivals raises the dirty bit, or
    /// finds the role free and passes itself, and reads what was released
    /// from the state.
    fn drain(&self) {
        let queue = self.queue();
        let mut woken = WakeList::new(queue);
        loop {
            let before = queue
                .state
                .fetch_and(!(DIRTY | WAKE_FRONT), Ordering::AcqRel);
            let mut clear = self.pass(queue.take(), &mut woken, before & WAKE_FRONT != 0);
            if queue.owed.load(Ordering::Acquire) == 0 {
                clear |= HANDOFF;
            }
            let (wanted, gone) = if clear & QUEUED != 0 {
                (0, DRAINING | WAKE_FRONT | clear)
            } else {
                (WAKE_FRONT, DRAINING | clear)
            };
            let mut current = queue.state.load(Ordering::Acquire);
            while current & (DIRTY | wanted) == 0 {
                match queue.state.compare_exchange_weak(
                    current,
                    current & !gone,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return,
                    Err(actual) => current = actual,
                }
            }
        }
    }

    /// Queues `wait` with `payload` and publishes it.
    fn enqueue(&self, wait: &mut Wait, payload: usize, waker: &core::task::Waker) {
        wait.enqueue(self.queue(), payload, waker);
        self.transition(|state| Step::Drain(state | QUEUED));
    }

    /// Tells the drainer a waiter withdrew, so it can prune it and
    /// reconsider whatever it was blocking. A nudge that was on its way to
    /// that waiter is void, so the marker goes too; if the waiter had been
    /// woken and never used the wake, the wake passes to the next front.
    fn withdrawn(&self, woken: bool) {
        self.queue().cancels.fetch_add(1, Ordering::Relaxed);
        let pass_on = if woken { WAKE_FRONT } else { 0 };
        self.transition(|state| Step::Drain((state & !NUDGE_OUT) | pass_on));
    }

    /// After a release that found waiters: wake the front whatever it
    /// asks for.
    fn released(&self) {
        self.transition(|state| Step::Wake(state | WAKE_FRONT));
    }

    /// The attempt that would be `wait`'s last counted loss. It marks the
    /// waiter owed, then in one compare-and-swap either takes what it asks
    /// for (`take` gives the state with it taken, or `None` when it does
    /// not fit) or raises [`HANDOFF`], so a release ordered after the loss
    /// finds barging shut and leaves what it frees for the drain to hand
    /// over. A take, or a handoff already owed to someone else (then the
    /// loss does not count), leaves the waiter un-owed. On failure, the
    /// state the attempt saw.
    fn owe_or_take(
        &self,
        wait: &Wait,
        mut take: impl FnMut(usize) -> Option<usize>,
    ) -> Result<(), usize> {
        let queue = self.queue();
        // Counted before the node is marked, so the drainer that settles
        // the mark never takes the count below zero, and before the bit
        // is raised, so a drain that sees the bit sees the count too.
        queue.owed.fetch_add(1, Ordering::AcqRel);
        let marked = wait.owe();
        let mut took = false;
        let seen = if marked {
            self.transition(|state| {
                took = false;
                if state & HANDOFF != 0 {
                    return Step::Keep;
                }
                match take(state) {
                    Some(next) => {
                        took = true;
                        Step::Set(next)
                    }
                    None => Step::Drain(state | HANDOFF),
                }
            })
        } else {
            // Granted already: the next look at the node finds it.
            queue.state.load(Ordering::Acquire)
        };
        if !marked || took || seen & HANDOFF != 0 {
            // A node already granted keeps no mark; the queue settles one
            // that a grant beat this to.
            if !marked || wait.unowe() {
                queue.forgive();
            }
            // A drain that counted this waiter meanwhile kept the bit up
            // for it, so one more pass decides. A read-modify-write, to
            // see that drain's exit if it came first.
            if queue.state.fetch_or(0, Ordering::AcqRel) & HANDOFF != 0 {
                self.transition(Step::Drain);
            }
        }
        if took { Ok(()) } else { Err(seen) }
    }
}
