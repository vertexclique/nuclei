//! Waiter nodes, the free list that recycles them, and the wake list a
//! drain pass hands woken waiters to.
//!
//! A contended future parks one [`Waiter`] in its primitive's queue. Up to
//! three parties hold the node: the future, until it completes or is
//! dropped; the queue, until a drain pass grants or prunes it; and a wake
//! list, while a waker taken from the node waits to be called. Each holds
//! one reference bit in the node's state word, and whichever party clears
//! the last bit returns the node to its primitive's [`Pool`]. No other path
//! frees a node, so a node is never reachable by a party that has let it
//! go, and no epoch scheme is needed.
//!
//! # State word
//!
//! | bits | meaning |
//! |---|---|
//! | 0-1 | phase: waiting, granted or cancelled; it leaves waiting once |
//! | 2 | registering: the future is writing a waker into a slot |
//! | 3 | armed: the current slot holds a waker nobody has taken |
//! | 4 | taking: a wake list is taking the waker from the taken slot |
//! | 5 | the current slot: where the future last registered |
//! | 6 | the taken slot: where the wake list takes from |
//! | 7 | nudged: a release asked the waiter to try again |
//! | 8 | owed: the waiter reached the bypass bound and is counted as owed |
//! | 9-11 | references: queue, future, wake list |
//!
//! A drainer *grants* a waiter (hands it what it waits for) or *nudges* it
//! (wakes it to compete again, leaving it in the queue). Either may claim
//! the armed waker: it marks the current slot as the taken one and hands
//! the node to a wake list, which takes the waker and calls it once the
//! drain role is given up.
//!
//! The waker lives in one of two slots so that neither side ever waits for
//! the other. The future writes a slot only while it holds the registering
//! bit, and it writes the slot that is not being taken; the wake list
//! reads only the taken slot, and only while it holds the taking bit. A
//! cancelling future reads the current slot only while it is armed. So no
//! two parties touch one slot at once. A grant or nudge that finds the
//! future registering, or a take already under way, only sets its bit: the
//! future reads that bit as it finishes registering, and the wake list,
//! once its take is done, claims a waker armed meanwhile whose node was
//! granted or nudged, and calls that one too.

#![allow(unsafe_code)]

use core::mem;
use core::ptr::{self, NonNull};
use core::task::{Poll, Waker};

use super::internal::{AtomicPtr, AtomicUsize, Ordering, UnsafeCell};
use super::queue::WaitQueue;
use super::spin::pause;

const PHASE: usize = 0b11;
const WAITING: usize = 0;
const GRANTED: usize = 1;
const CANCELLED: usize = 2;
const REGISTERING: usize = 1 << 2;
const ARMED: usize = 1 << 3;
const TAKING: usize = 1 << 4;
const CURRENT: usize = 1 << 5;
const TAKEN: usize = 1 << 6;
const NUDGED: usize = 1 << 7;
const OWED: usize = 1 << 8;
const QUEUE_REF: usize = 1 << 9;
const FUTURE_REF: usize = 1 << 10;
const WAKE_REF: usize = 1 << 11;
const REFS: usize = QUEUE_REF | FUTURE_REF | WAKE_REF;

/// Nodes a primitive keeps for reuse; past this the allocator gets them
/// back, so a burst of contention cannot pin memory forever.
#[cfg(not(loom))]
const POOL_CAPACITY: usize = 16;

/// One parked wait.
pub(super) struct Waiter {
    state: AtomicUsize,
    next: AtomicPtr<Waiter>,
    wake_next: AtomicPtr<Waiter>,
    payload: AtomicUsize,
    slots: [UnsafeCell<Option<Waker>>; 2],
}

/// What a grant or a nudge did to a node.
enum Touch {
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
enum Seen {
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
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            next: AtomicPtr::new(ptr::null_mut()),
            wake_next: AtomicPtr::new(ptr::null_mut()),
            payload: AtomicUsize::new(0),
            slots: [UnsafeCell::new(None), UnsafeCell::new(None)],
        }
    }

    /// The link the arrival stack or a waiting list threads through; only
    /// the party holding the node in that structure uses it.
    pub(super) fn next(&self) -> *mut Waiter {
        self.next.load(Ordering::Relaxed)
    }

    pub(super) fn set_next(&self, next: *mut Waiter) {
        self.next.store(next, Ordering::Relaxed);
    }

    /// What the waiter asked for: a permit count, a lock kind, a
    /// generation. Written before the node is published, so a drainer
    /// that took the node sees it.
    pub(super) fn payload(&self) -> usize {
        self.payload.load(Ordering::Relaxed)
    }

    /// Overwrites the payload. A drainer uses this before a grant to tell
    /// the future how it was granted; the grant publishes the store.
    pub(super) fn set_payload(&self, payload: usize) {
        self.payload.store(payload, Ordering::Relaxed);
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) & PHASE == CANCELLED
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
    fn arm(&self, payload: usize, waker: Waker) {
        self.state.store(
            WAITING | REGISTERING | QUEUE_REF | FUTURE_REF,
            Ordering::Relaxed,
        );
        self.next.store(ptr::null_mut(), Ordering::Relaxed);
        self.wake_next.store(ptr::null_mut(), Ordering::Relaxed);
        self.payload.store(payload, Ordering::Relaxed);
        // SAFETY: the node came out of the pool, so nobody else sees it.
        self.slots[0].with_mut(|slot| unsafe { *slot = Some(waker) });
    }

    /// Sets `bit` on a waiting node, claiming its waker if it can. `once`
    /// leaves a node that already carries `bit` alone and reports it as
    /// [`Touch::Pending`].
    fn touch(&self, bit: usize, once: bool) -> Touch {
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
    fn park(&self, waker: Option<&Waker>) -> Seen {
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
    fn begin_spin(&self) -> bool {
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
    fn marked(&self) -> bool {
        let state = self.state.load(Ordering::Relaxed);
        state & PHASE == GRANTED || state & NUDGED != 0
    }

    /// Marks a waiting node as owed a handoff. False when it already left
    /// the waiting phase.
    fn owe(&self) -> bool {
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
    fn unowe(&self) -> bool {
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
    fn withdraw(&self) -> Option<(Option<Waker>, bool)> {
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
    fn take_claimed(&self) -> (Option<Waker>, Taken) {
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
    fn clear_slots(&self) {
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
    fn retire(&mut self) {
        self.state.with_mut(|_| ());
        self.next.with_mut(|_| ());
        self.wake_next.with_mut(|_| ());
        self.payload.with_mut(|_| ());
    }
}

/// Loom-only instruments of the spin on the waiter's node: what the
/// models in `loom_model.rs` count to show they reach each branch of the
/// spin, and the one guard their calibration takes away. Loom runs a model's
/// threads as coroutines on the test's own thread, so these are per test.
#[cfg(loom)]
pub(super) mod spin_probe {
    use core::cell::Cell;
    use std::thread::LocalKey;

    std::thread_local! {
        /// Spins on a node that began.
        pub(in super::super) static SPINS: Cell<usize> = const { Cell::new(0) };
        /// Spins that saw a mark land on the unarmed node.
        pub(in super::super) static MARKED_WHILE_SPINNING: Cell<usize> = const { Cell::new(0) };
        /// Spins that ran out, after which only the look found a mark.
        pub(in super::super) static MARKED_AFTER_LAST_READ: Cell<usize> = const { Cell::new(0) };
        /// The calibration's switch: a spin that runs out arms the waker
        /// without the look's reading of the marks.
        pub(in super::super) static SKIP_LOOK: Cell<bool> = const { Cell::new(false) };
    }

    /// Adds one to `counter`.
    pub(in super::super) fn count(counter: &'static LocalKey<Cell<usize>>) {
        counter.with(|c| c.set(c.get() + 1));
    }

    /// Reads and resets the three counts, in the order above.
    pub(in super::super) fn take() -> [usize; 3] {
        [&SPINS, &MARKED_WHILE_SPINNING, &MARKED_AFTER_LAST_READ].map(|c| c.with(|c| c.replace(0)))
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
    fn arm_without_look(&self, waker: &Waker) -> Park {
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
enum Taken {
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
fn release(node: NonNull<Waiter>, bit: usize, queue: &WaitQueue) {
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
pub(super) fn release_queued(node: NonNull<Waiter>, queue: &WaitQueue) {
    release(node, QUEUE_REF, queue);
}

/// A pooled node, owned by whoever holds this value.
#[cfg(not(loom))]
struct Pooled(NonNull<Waiter>);

// SAFETY: a pooled node is referenced by nothing but its `Pooled`.
#[cfg(not(loom))]
unsafe impl Send for Pooled {}

/// A primitive's free list of waiter nodes.
///
/// The ring is kovan's bounded lock-free MPMC queue, allocated the first
/// time a node is returned, so a primitive that never contends never
/// allocates. Under `--cfg loom` the pool is a pass-through to the
/// allocator: loom cannot see inside kovan's queue, and a hand-off it
/// cannot see would read to it as a data race.
pub(super) struct Pool {
    #[cfg(not(loom))]
    ring: core::sync::atomic::AtomicPtr<kovan_queue::array_queue::ArrayQueue<Pooled>>,
}

impl Pool {
    pub(super) const fn new() -> Self {
        Self {
            #[cfg(not(loom))]
            ring: core::sync::atomic::AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn take(&self) -> NonNull<Waiter> {
        #[cfg(not(loom))]
        {
            let ring = self.ring.load(Ordering::Acquire);
            // SAFETY: a published ring lives until the pool drops.
            if let Some(Pooled(node)) = unsafe { ring.as_ref() }.and_then(|ring| ring.pop()) {
                return node;
            }
        }
        NonNull::from(Box::leak(Box::new(Waiter::new())))
    }

    fn put(&self, node: NonNull<Waiter>) {
        // SAFETY: every reference is gone, so the caller owns the node.
        unsafe { node.as_ref() }.clear_slots();
        #[cfg(not(loom))]
        let node = match self.ring().push(Pooled(node)) {
            Ok(()) => return,
            Err(Pooled(node)) => node,
        };
        // SAFETY: as above, the caller owns the node outright.
        #[cfg(loom)]
        unsafe { &mut *node.as_ptr() }.retire();
        // SAFETY: every node is a leaked `Box` and this one is unreferenced.
        drop(unsafe { Box::from_raw(node.as_ptr()) });
    }

    #[cfg(not(loom))]
    fn ring(&self) -> &kovan_queue::array_queue::ArrayQueue<Pooled> {
        let ring = self.ring.load(Ordering::Acquire);
        // SAFETY: a published ring lives until the pool drops.
        if let Some(ring) = unsafe { ring.as_ref() } {
            return ring;
        }
        let fresh = Box::into_raw(Box::new(kovan_queue::array_queue::ArrayQueue::new(
            POOL_CAPACITY,
        )));
        match self.ring.compare_exchange(
            ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: just published; it lives until the pool drops.
            Ok(_) => unsafe { &*fresh },
            Err(winner) => {
                // SAFETY: `fresh` lost the race and was never shared.
                drop(unsafe { Box::from_raw(fresh) });
                // SAFETY: as above, for the winner's ring.
                unsafe { &*winner }
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        #[cfg(not(loom))]
        {
            let ring = *self.ring.get_mut();
            if !ring.is_null() {
                // SAFETY: the pool owns its ring and is being dropped.
                let ring = unsafe { Box::from_raw(ring) };
                while let Some(Pooled(node)) = ring.pop() {
                    // SAFETY: a pooled node is an unreferenced leaked `Box`.
                    drop(unsafe { Box::from_raw(node.as_ptr()) });
                }
            }
        }
    }
}

/// Waiters a drain pass granted or nudged, woken in order once the pass
/// has given up the drain role, so no executor code runs while it holds
/// it.
pub(super) struct WakeList<'a> {
    head: *mut Waiter,
    tail: *mut Waiter,
    queue: &'a WaitQueue,
}

impl<'a> WakeList<'a> {
    pub(super) fn new(queue: &'a WaitQueue) -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            queue,
        }
    }

    /// Grants `node`, which the caller took off its waiting list with the
    /// queue's reference. Returns false when the future had withdrawn, in
    /// which case the caller gives back whatever it set aside for it.
    pub(super) fn grant(&mut self, node: NonNull<Waiter>) -> bool {
        // SAFETY: the caller holds the queue's reference.
        let touch = unsafe { node.as_ref() }.touch(GRANTED, false);
        if let Touch::Claimed = touch {
            self.push(node);
        }
        release_queued(node, self.queue);
        !matches!(touch, Touch::Skipped)
    }

    /// Nudges `node`, which stays on its waiting list: it is woken to try
    /// again. Returns whether a nudge is on its way to it, this one or an
    /// earlier one its future has not looked at yet. An earlier one counts:
    /// a caller that took it for "no nudge" would lower the marker of a
    /// nudge that is still out, and every release would then ask for a pass
    /// until that waiter polls, which a waiter that holds the drain role
    /// itself cannot do while those passes keep coming.
    pub(super) fn nudge(&mut self, node: NonNull<Waiter>) -> bool {
        // SAFETY: the caller's list holds the queue's reference.
        match unsafe { node.as_ref() }.touch(NUDGED, true) {
            Touch::Claimed => {
                self.push(node);
                true
            }
            Touch::Marked | Touch::Pending => true,
            Touch::Skipped => false,
        }
    }

    fn push(&mut self, node: NonNull<Waiter>) {
        // SAFETY: the claim gave this list the node's wake reference.
        unsafe { node.as_ref() }
            .wake_next
            .store(ptr::null_mut(), Ordering::Relaxed);
        match NonNull::new(self.tail) {
            // SAFETY: the tail is a node this list holds.
            Some(tail) => unsafe { tail.as_ref() }
                .wake_next
                .store(node.as_ptr(), Ordering::Relaxed),
            None => self.head = node.as_ptr(),
        }
        self.tail = node.as_ptr();
    }

    fn pop(&mut self) -> Option<Option<Waker>> {
        let node = NonNull::new(self.head)?;
        // SAFETY: this list holds the node's wake reference.
        let waiter = unsafe { node.as_ref() };
        // The link is read while this list still holds the node. The take
        // below may let go of the last reference but one, and the party
        // holding that one may then free or reuse the node at once. Nobody
        // else writes the link meanwhile: the taking bit keeps every other
        // wake list from claiming the node until the take lets it go.
        let next = waiter.wake_next.load(Ordering::Relaxed);
        let (waker, taken) = waiter.take_claimed();
        match taken {
            // The node stays at the front for the waker just claimed.
            Taken::Again => {}
            Taken::Done(last) => {
                self.head = next;
                if self.head.is_null() {
                    self.tail = ptr::null_mut();
                }
                if last {
                    self.queue.pool().put(node);
                }
            }
        }
        Some(waker)
    }
}

impl Drop for WakeList<'_> {
    fn drop(&mut self) {
        while let Some(waker) = self.pop() {
            if let Some(waker) = waker {
                // A waker that panics must not strand the waiters behind
                // it: they already own what they were granted, or must
                // still be told to try again.
                let rest = WakeRest(self);
                waker.wake();
                mem::forget(rest);
            }
        }
    }
}

struct WakeRest<'b, 'a>(&'b mut WakeList<'a>);

impl Drop for WakeRest<'_, '_> {
    fn drop(&mut self) {
        while let Some(waker) = self.0.pop() {
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

/// How a cancelled future left its queue.
pub(super) enum Cancel {
    /// It never queued, or it already completed.
    Idle,
    /// It withdrew before anyone granted it; its owner tells the queue,
    /// passing on the nudge it carries unused, if it does.
    Withdrawn(bool),
    /// It had been granted, so its owner passes the grant on. Carries the
    /// payload the drainer left.
    Granted(usize),
}

/// What a future's look at its node found.
pub(super) enum Park {
    /// Granted, with the payload the drainer left; the node is released.
    Granted(usize),
    /// A release asked it to try again; it is still queued.
    Nudged,
    /// Still waiting, its waker registered.
    Pending,
}

/// A future's hold on its waiter node.
pub(super) struct Wait {
    node: Option<NonNull<Waiter>>,
}

impl Wait {
    pub(super) const fn new() -> Self {
        Self { node: None }
    }

    #[inline]
    pub(super) fn is_queued(&self) -> bool {
        self.node.is_some()
    }

    /// Arms a node from `queue`'s pool and pushes it on the arrival
    /// stack. The caller then publishes it with a draining transition and
    /// settles it with [`park`](Self::park) and no waker.
    pub(super) fn enqueue(&mut self, queue: &WaitQueue, payload: usize, waker: &Waker) {
        debug_assert!(self.node.is_none(), "a future queues twice");
        let node = queue.pool().take();
        // SAFETY: fresh from the pool, so this future owns it alone.
        unsafe { node.as_ref() }.arm(payload, waker.clone());
        queue.push(node);
        self.node = Some(node);
    }

    /// Looks at the node; with `waker`, registers it first. Right after
    /// [`enqueue`](Self::enqueue) the waker is already in place, so pass
    /// `None`.
    pub(super) fn park(&mut self, queue: &WaitQueue, waker: Option<&Waker>) -> Park {
        let Some(node) = self.node else {
            return Park::Granted(0);
        };
        // SAFETY: this future holds its reference until it clears `node`.
        let waiter = unsafe { node.as_ref() };
        match waiter.park(waker) {
            Seen::Granted => {
                let payload = waiter.payload();
                self.node = None;
                release(node, FUTURE_REF, queue);
                Park::Granted(payload)
            }
            Seen::Nudged => Park::Nudged,
            Seen::Pending => Park::Pending,
        }
    }

    /// After a lost retry: spins on the node for up to the queue's
    /// budget, then looks at it as [`park`](Self::park) with `waker` does.
    /// A grant or nudge that comes during the spin finds the node unarmed,
    /// so it only sets its mark, and the spin sees the mark; if the spin
    /// runs out, the look arms the waker and reads the marks in one
    /// compare-and-swap, so a mark set as the spin gave up is not lost.
    pub(super) fn spin_park(&mut self, queue: &WaitQueue, waker: &Waker) -> Park {
        if let Some(node) = self.node {
            let rounds = queue.spin.rounds();
            // SAFETY: this future holds its reference until it clears `node`.
            let waiter = unsafe { node.as_ref() };
            if rounds > 0 && waiter.begin_spin() {
                #[cfg(loom)]
                spin_probe::count(&spin_probe::SPINS);
                if (0..rounds).any(|_| {
                    pause();
                    waiter.marked()
                }) {
                    // A grant or nudge landed on the node while it was
                    // unarmed, and the spin saw it.
                    #[cfg(loom)]
                    spin_probe::count(&spin_probe::MARKED_WHILE_SPINNING);
                    queue.spin.won(rounds);
                } else {
                    queue.spin.ran_out(rounds);
                    #[cfg(loom)]
                    {
                        if spin_probe::SKIP_LOOK.with(core::cell::Cell::get) {
                            return waiter.arm_without_look(waker);
                        }
                        let park = self.park(queue, Some(waker));
                        if !matches!(park, Park::Pending) {
                            // A mark landed after the spin's last read: only
                            // the look's swap saw it.
                            spin_probe::count(&spin_probe::MARKED_AFTER_LAST_READ);
                        }
                        return park;
                    }
                }
            }
        }
        self.park(queue, Some(waker))
    }

    /// For a primitive that only grants: ready once granted.
    pub(super) fn poll(&mut self, queue: &WaitQueue, waker: Option<&Waker>) -> Poll<usize> {
        match self.park(queue, waker) {
            Park::Granted(payload) => Poll::Ready(payload),
            Park::Nudged | Park::Pending => Poll::Pending,
        }
    }

    /// Marks the node owed a handoff. False when it already left the
    /// waiting phase.
    pub(super) fn owe(&self) -> bool {
        let Some(node) = self.node else {
            return false;
        };
        // SAFETY: this future holds its reference.
        unsafe { node.as_ref() }.owe()
    }

    /// Takes back the node's owed mark. False when it already left the
    /// waiting phase, and the queue settles the mark.
    pub(super) fn unowe(&self) -> bool {
        let Some(node) = self.node else {
            return false;
        };
        // SAFETY: this future holds its reference.
        unsafe { node.as_ref() }.unowe()
    }

    /// Leaves the queue: the future was dropped, or took what it waits
    /// for on its own. Reports a grant that got there first.
    pub(super) fn cancel(&mut self, queue: &WaitQueue) -> Cancel {
        let Some(node) = self.node.take() else {
            return Cancel::Idle;
        };
        // SAFETY: this future held its reference until now.
        let waiter = unsafe { node.as_ref() };
        let outcome = match waiter.withdraw() {
            Some((waker, nudged)) => {
                drop(waker);
                Cancel::Withdrawn(nudged)
            }
            None => Cancel::Granted(waiter.payload()),
        };
        release(node, FUTURE_REF, queue);
        outcome
    }
}

// SAFETY: the node is shared only through the protocol in the module
// docs, which every access follows whichever thread makes it.
unsafe impl Send for Wait {}
// SAFETY: `&Wait` exposes nothing.
unsafe impl Sync for Wait {}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::super::test_support::Polled;
    use super::*;

    /// A queue with one waiter armed, as after a poll that returned
    /// `Pending`, and the waker that counts its wakes.
    fn armed() -> (WaitQueue, Wait, Polled<core::future::Pending<()>>) {
        let queue = WaitQueue::new(0);
        let counter = Polled::new(core::future::pending::<()>());
        let mut wait = Wait::new();
        wait.enqueue(&queue, 1, &counter.waker());
        assert!(matches!(wait.park(&queue, None), Park::Pending));
        (queue, wait, counter)
    }

    fn node(wait: &Wait) -> NonNull<Waiter> {
        wait.node.expect("queued")
    }

    /// B, the spin on the waiter's own node. A nudge that finds the waiter
    /// spinning (disarmed by `begin_spin`) sets the mark and calls no
    /// waker; the look that ends the spin finds the mark.
    #[test]
    fn a_nudge_that_finds_its_waiter_spinning_marks_it_and_wakes_nobody() {
        let (queue, mut wait, counter) = armed();
        // SAFETY: the future holds its reference for the whole test.
        let waiter = unsafe { node(&wait).as_ref() };
        assert!(waiter.begin_spin(), "nothing marked yet: the waiter spins");
        let mut woken = WakeList::new(&queue);
        assert!(woken.nudge(node(&wait)), "a nudge is on its way");
        drop(woken);
        assert_eq!(
            counter.wakes(),
            0,
            "a spinning waiter's waker is not called"
        );
        assert!(waiter.marked(), "the spin sees the mark");
        assert!(matches!(
            wait.park(&queue, Some(&counter.waker())),
            Park::Nudged
        ));
        assert!(matches!(wait.cancel(&queue), Cancel::Withdrawn(false)));
    }

    /// The same nudge to an armed waiter claims its waker and calls it: what
    /// `begin_spin` saves a spinning waiter.
    #[test]
    fn a_nudge_that_finds_its_waiter_armed_calls_its_waker() {
        let (queue, mut wait, counter) = armed();
        let mut woken = WakeList::new(&queue);
        assert!(woken.nudge(node(&wait)));
        drop(woken);
        assert_eq!(counter.wakes(), 1, "an armed waiter is woken");
        assert!(matches!(
            wait.park(&queue, Some(&counter.waker())),
            Park::Nudged
        ));
        assert!(matches!(wait.cancel(&queue), Cancel::Withdrawn(false)));
    }

    /// A mark set just as the spin gives up is not lost: the look that arms
    /// the waker again reads the marks in the same compare-and-swap, so the
    /// waiter retries instead of sleeping beside it.
    #[test]
    fn a_nudge_as_the_spin_gives_up_is_seen_by_the_look() {
        let (queue, mut wait, counter) = armed();
        // SAFETY: as above.
        let waiter = unsafe { node(&wait).as_ref() };
        assert!(waiter.begin_spin());
        assert!(!waiter.marked(), "the spin runs out with nothing marked");
        let mut woken = WakeList::new(&queue);
        assert!(woken.nudge(node(&wait)), "the nudge lands as it gives up");
        drop(woken);
        assert_eq!(counter.wakes(), 0);
        assert!(
            matches!(wait.park(&queue, Some(&counter.waker())), Park::Nudged),
            "the look found the nudge; the waiter does not sleep beside it"
        );
        assert!(matches!(wait.cancel(&queue), Cancel::Withdrawn(false)));
    }

    /// A node marked before the spin begins needs no spin: `begin_spin`
    /// says so and leaves the mark for the look.
    #[test]
    fn a_marked_node_does_not_spin() {
        let (queue, mut wait, counter) = armed();
        let mut woken = WakeList::new(&queue);
        assert!(woken.nudge(node(&wait)));
        drop(woken);
        // SAFETY: as above.
        assert!(!unsafe { node(&wait).as_ref() }.begin_spin());
        assert!(matches!(
            wait.park(&queue, Some(&counter.waker())),
            Park::Nudged
        ));
        assert!(matches!(wait.cancel(&queue), Cancel::Withdrawn(false)));
    }
}
