//! The waiter node and the state machine behind it: phases, the two waker
//! slots, the marks a drainer leaves and the reference bits that decide who
//! returns the node to its pool.
//!
//! See the [module docs](super) for the state word and the protocol.

#![allow(unsafe_code)]

use core::ptr::{self, NonNull};
use core::task::Waker;

#[cfg(loom)]
use super::wait::Park;
use crate::sync::internal::{AtomicPtr, AtomicUsize, Ordering, UnsafeCell};
use crate::sync::queue::WaitQueue;

const PHASE: usize = 0b11;
const WAITING: usize = 0;
pub(super) const GRANTED: usize = 1;
const CANCELLED: usize = 2;
const REGISTERING: usize = 1 << 2;
const ARMED: usize = 1 << 3;
const TAKING: usize = 1 << 4;
const CURRENT: usize = 1 << 5;
const TAKEN: usize = 1 << 6;
pub(super) const NUDGED: usize = 1 << 7;
const OWED: usize = 1 << 8;
const QUEUE_REF: usize = 1 << 9;
pub(super) const FUTURE_REF: usize = 1 << 10;
const WAKE_REF: usize = 1 << 11;
const REFS: usize = QUEUE_REF | FUTURE_REF | WAKE_REF;

/// One parked wait.
pub(in crate::sync) struct Waiter {
    state: AtomicUsize,
    next: AtomicPtr<Waiter>,
    prev: AtomicPtr<Waiter>,
    pub(super) wake_next: AtomicPtr<Waiter>,
    payload: AtomicUsize,
    slots: [UnsafeCell<Option<Waker>>; 2],
}

/// What a grant or a nudge did to a node.
pub(super) enum Touch {
    /// It claimed the armed waker; the node is on its way to a wake list.
    Claimed,
    /// It set its bit without the waker: the future is registering, its
    /// waker was already taken, or a take is under way.
    Marked,
    /// It did nothing, because the node still waits and already carries
    /// the bit: an earlier nudge is on its way and the future has not
    /// looked yet.
    Pending,
    /// It did nothing: the node left the waiting phase.
    Skipped,
}

/// What a future found when it looked at its node.
pub(super) enum Seen {
    Granted,
    Nudged,
    Pending,
}

/// `state` with the armed waker claimed for a wake list: the current slot
/// becomes the taken one.
fn claim(state: usize) -> usize {
    let taken = if state & CURRENT != 0 { TAKEN } else { 0 };
    (state & !(ARMED | TAKEN)) | taken | TAKING | WAKE_REF
}

/// Whether a waker can be claimed from `state`: armed, and nobody is
/// registering or taking.
fn claimable(state: usize) -> bool {
    state & (ARMED | REGISTERING | TAKING) == ARMED
}

impl Waiter {
    pub(super) fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            next: AtomicPtr::new(ptr::null_mut()),
            prev: AtomicPtr::new(ptr::null_mut()),
            wake_next: AtomicPtr::new(ptr::null_mut()),
            payload: AtomicUsize::new(0),
            slots: [UnsafeCell::new(None), UnsafeCell::new(None)],
        }
    }

    /// The link the arrival stack or a waiting list threads through; only
    /// the party holding the node in that structure uses it.
    pub(in crate::sync) fn next(&self) -> *mut Waiter {
        self.next.load(Ordering::Relaxed)
    }

    pub(in crate::sync) fn set_next(&self, next: *mut Waiter) {
        self.next.store(next, Ordering::Relaxed);
    }

    /// The back link of a node on a [`List`](super::list::List); only the
    /// drain role's holder, which owns the list, uses it.
    pub(in crate::sync) fn prev(&self) -> *mut Waiter {
        self.prev.load(Ordering::Relaxed)
    }

    pub(in crate::sync) fn set_prev(&self, prev: *mut Waiter) {
        self.prev.store(prev, Ordering::Relaxed);
    }

    /// What the waiter asked for: a permit count, a lock kind, a
    /// generation. Written before the node is published, so a drainer
    /// that took the node sees it.
    pub(in crate::sync) fn payload(&self) -> usize {
        self.payload.load(Ordering::Relaxed)
    }

    /// Overwrites the payload. A drainer uses this before a grant to tell
    /// the future how it was granted; the grant publishes the store.
    pub(in crate::sync) fn set_payload(&self, payload: usize) {
        self.payload.store(payload, Ordering::Relaxed);
    }

    pub(in crate::sync) fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) & PHASE == CANCELLED
    }

    /// Whether a drainer has granted the node.
    pub(super) fn is_granted(&self) -> bool {
        self.state.load(Ordering::Acquire) & PHASE == GRANTED
    }

    fn is_owed(&self) -> bool {
        self.state.load(Ordering::Acquire) & OWED != 0
    }

    /// The slot `bit` (current or taken) of `state` names.
    fn slot(&self, state: usize, bit: usize) -> &UnsafeCell<Option<Waker>> {
        &self.slots[usize::from(state & bit != 0)]
    }

    /// Fills a fresh node. It starts registering, since its future is
    /// mid-poll: a grant or nudge during the enqueue sets its bit without
    /// a wake, and [`park`](Self::park) with no waker settles it.
    pub(super) fn arm(&self, payload: usize, waker: Waker) {
        self.state.store(
            WAITING | REGISTERING | QUEUE_REF | FUTURE_REF,
            Ordering::Relaxed,
        );
        self.next.store(ptr::null_mut(), Ordering::Relaxed);
        self.prev.store(ptr::null_mut(), Ordering::Relaxed);
        self.wake_next.store(ptr::null_mut(), Ordering::Relaxed);
        self.payload.store(payload, Ordering::Relaxed);
        // SAFETY: the node came out of the pool, so nobody else sees it.
        self.slots[0].with_mut(|slot| unsafe { *slot = Some(waker) });
    }

    /// Sets `bit` on a waiting node, claiming its waker if it can. `once`
    /// leaves a node that already carries `bit` alone and reports it as
    /// [`Touch::Pending`].
    pub(super) fn touch(&self, bit: usize, once: bool) -> Touch {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & PHASE != WAITING {
                return Touch::Skipped;
            }
            if once && state & bit != 0 {
                return Touch::Pending;
            }
            let take = claimable(state);
            let next = if take {
                claim(state) | bit
            } else {
                state | bit
            };
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) if take => return Touch::Claimed,
                Ok(_) => return Touch::Marked,
                Err(actual) => state = actual,
            }
        }
    }

    /// Registers `waker` unless the node is granted or nudged, consuming a
    /// pending nudge. With no waker, settles a node fresh from
    /// [`arm`](Self::arm), whose waker is already in the slot.
    pub(super) fn park(&self, waker: Option<&Waker>) -> Seen {
        if let Some(waker) = waker {
            let mut state = self.state.load(Ordering::Acquire);
            let target = loop {
                if state & PHASE == GRANTED {
                    return Seen::Granted;
                }
                if state & NUDGED != 0 {
                    match self.state.compare_exchange_weak(
                        state,
                        state & !NUDGED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Seen::Nudged,
                        Err(actual) => {
                            state = actual;
                            continue;
                        }
                    }
                }
                // Write the slot a take under way is not reading.
                let current = if state & TAKING != 0 {
                    !state & TAKEN != 0
                } else {
                    state & CURRENT != 0
                };
                let next =
                    (state & !(ARMED | CURRENT)) | REGISTERING | if current { CURRENT } else { 0 };
                match self.state.compare_exchange_weak(
                    state,
                    next,
                    Ordering::Acquire,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break next,
                    Err(actual) => state = actual,
                }
            };
            // SAFETY: the registering bit makes the current slot this
            // future's, and a take reads only the other one (module docs).
            self.slot(target, CURRENT).with_mut(|slot| unsafe {
                if !(*slot).as_ref().is_some_and(|w| w.will_wake(waker)) {
                    *slot = Some(waker.clone());
                }
            });
        }
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            let next = (state & !(REGISTERING | NUDGED)) | ARMED;
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) if state & PHASE == GRANTED => return Seen::Granted,
                Ok(_) if state & NUDGED != 0 => return Seen::Nudged,
                Ok(_) => return Seen::Pending,
                Err(actual) => state = actual,
            }
        }
    }

    /// Readies the node for its future to spin on, inside a poll: a grant
    /// or nudge that comes while it spins must only set its mark, never
    /// claim the waker. So an armed waker is disarmed; it stays in its slot
    /// for [`park`](Self::park) to arm again, which reads the marks in the
    /// same compare-and-swap. False when the node is granted or nudged
    /// already, so there is nothing to wait for. In the common case, a
    /// future woken by the claim of its waker, nothing is armed and this is
    /// one load.
    pub(super) fn begin_spin(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & PHASE == GRANTED || state & NUDGED != 0 {
                return false;
            }
            if state & ARMED == 0 {
                return true;
            }
            match self.state.compare_exchange_weak(
                state,
                state & !ARMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => state = actual,
            }
        }
    }

    /// Whether a grant or nudge has reached the node: what a spin waits
    /// for. A plain load; the look that follows the spin synchronizes.
    pub(super) fn marked(&self) -> bool {
        let state = self.state.load(Ordering::Relaxed);
        state & PHASE == GRANTED || state & NUDGED != 0
    }

    /// Marks a waiting node as owed a handoff. False when it already left
    /// the waiting phase.
    pub(super) fn owe(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & PHASE != WAITING {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state | OWED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => state = actual,
            }
        }
    }

    /// Takes back the owed mark of a node still waiting. False when it
    /// left the waiting phase first, so the queue settles the mark as it
    /// lets the node go.
    pub(super) fn unowe(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & PHASE != WAITING || state & OWED == 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state & !OWED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => state = actual,
            }
        }
    }

    /// Moves a waiting node to cancelled, taking its armed waker, and
    /// reports whether a nudge had reached it unused. `None` when it was
    /// already granted.
    pub(super) fn withdraw(&self) -> Option<(Option<Waker>, bool)> {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & PHASE == GRANTED {
                return None;
            }
            // The future is not registering: it is being dropped.
            let armed = state & ARMED != 0;
            let nudged = state & NUDGED != 0;
            match self.state.compare_exchange_weak(
                state,
                (state & !ARMED) | CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                // SAFETY: an armed current slot is the future's, and a take
                // under way reads the other one (module docs).
                Ok(_) if armed => {
                    let waker = self
                        .slot(state, CURRENT)
                        .with_mut(|slot| unsafe { (*slot).take() });
                    return Some((waker, nudged));
                }
                Ok(_) => return Some((None, nudged)),
                Err(actual) => state = actual,
            }
        }
    }

    /// Takes the waker a claim set aside, then gives the slot back. If a
    /// grant or nudge marked a waker armed meanwhile, claims that one too
    /// and reports [`Taken::Again`]. The caller holds the taking bit. After
    /// [`Taken::Done`] the caller has let go of the node: unless it held
    /// the last reference, it must not touch the node again, since the
    /// party holding the last one may free or reuse it at once.
    pub(super) fn take_claimed(&self) -> (Option<Waker>, Taken) {
        let mut state = self.state.load(Ordering::Acquire);
        // SAFETY: the taking bit makes the taken slot this caller's.
        let waker = self
            .slot(state, TAKEN)
            .with_mut(|slot| unsafe { (*slot).take() });
        loop {
            let again = state & (ARMED | REGISTERING) == ARMED
                && (state & NUDGED != 0 || state & PHASE == GRANTED);
            let next = if again {
                claim(state & !TAKING)
            } else {
                state & !(TAKING | WAKE_REF)
            };
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) if again => return (waker, Taken::Again),
                Ok(_) => return (waker, Taken::Done(state & REFS == WAKE_REF)),
                Err(actual) => state = actual,
            }
        }
    }

    /// Drops whatever wakers the slots still hold. The caller owns the node
    /// outright.
    pub(super) fn clear_slots(&self) {
        for slot in &self.slots {
            // SAFETY: forwarded from the caller.
            drop(slot.with_mut(|slot| unsafe { (*slot).take() }));
        }
    }

    /// Under loom, freeing the node counts as a write to every word of it.
    /// loom then checks that each earlier access, by any party, happened
    /// before the free, so a party that touches the node after letting go
    /// of its reference fails the model instead of reading freed memory
    /// unseen.
    #[cfg(loom)]
    pub(super) fn retire(&mut self) {
        self.state.with_mut(|_| ());
        self.next.with_mut(|_| ());
        self.prev.with_mut(|_| ());
        self.wake_next.with_mut(|_| ());
        self.payload.with_mut(|_| ());
    }
}

#[cfg(loom)]
impl Waiter {
    /// The calibration's broken end of a spin: registers `waker` and arms it
    /// without reading the marks, which is what `park` does in one swap
    /// with the reading. A node that carries an unread mark at the arming
    /// swap now sleeps beside it: no release will wake it, since the one
    /// that marked it found it unarmed and the nudge marker stays up. The
    /// model stops there rather than park for good, which loom would report
    /// as a deadlock and abort the test binary with.
    pub(super) fn arm_without_look(&self, waker: &Waker) -> Park {
        let mut state = self.state.load(Ordering::Acquire);
        let target = loop {
            let current = if state & TAKING != 0 {
                !state & TAKEN != 0
            } else {
                state & CURRENT != 0
            };
            let next =
                (state & !(ARMED | CURRENT)) | REGISTERING | if current { CURRENT } else { 0 };
            match self.state.compare_exchange_weak(
                state,
                next,
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => break next,
                Err(actual) => state = actual,
            }
        };
        // SAFETY: as in `park`: the registering bit makes the slot this
        // future's.
        self.slot(target, CURRENT).with_mut(|slot| unsafe {
            if !(*slot).as_ref().is_some_and(|w| w.will_wake(waker)) {
                *slot = Some(waker.clone());
            }
        });
        let before = self.state.fetch_xor(REGISTERING | ARMED, Ordering::AcqRel);
        assert!(
            before & NUDGED == 0 && before & PHASE != GRANTED,
            "the waiter armed and slept beside an unread mark"
        );
        Park::Pending
    }
}

/// What [`Waiter::take_claimed`] left behind.
pub(super) enum Taken {
    /// Another waker was claimed; take again.
    Again,
    /// The wake list let go of the node; `true` when it was the last
    /// reference, so the node goes back to the pool.
    Done(bool),
}

/// Drops one party's reference; the last party out recycles the node.
/// Letting go of the queue's reference also settles the node's share of
/// the owed count.
///
/// The caller must hold `bit` on `node`, and a node the queue lets go of
/// has left the waiting phase, so its owed bit no longer changes.
pub(super) fn release(node: NonNull<Waiter>, bit: usize, queue: &WaitQueue) {
    // SAFETY: the caller still holds `bit`, so the node is live.
    let waiter = unsafe { node.as_ref() };
    if bit == QUEUE_REF && waiter.is_owed() {
        queue.forgive();
    }
    let prev = waiter.state.fetch_and(!bit, Ordering::AcqRel);
    if prev & REFS == bit {
        queue.pool().put(node);
    }
}

/// Lets go of the queue's reference to `node`, which the caller took off
/// its arrival stack or waiting list, and which is not waiting any more.
pub(in crate::sync) fn release_queued(node: NonNull<Waiter>, queue: &WaitQueue) {
    release(node, QUEUE_REF, queue);
}
