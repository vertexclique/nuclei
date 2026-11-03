//! The `block_on` of a runtime other than the one a shard serves, nested on
//! it. The shard's loop goes on under the runtime it serves: its tasks, its
//! timers and its IO read that runtime's clock. The nested future is polled
//! under its own runtime, with that runtime's wheels swapped in for the poll,
//! so its sleeps read its clock and fire by it. Between the polls those wheels
//! are kept aside on the shard, and the loop fires them by their runtime's
//! clock through a count on the precise wheel, which sends the rare branches
//! of the timer pass here (`proofs/tla/clock/OneWheel.tla`).
//!
//! Only a thread's own shard swaps: nobody watches it, so nobody takes it
//! while a nested poll holds the other wheels. A worker's shard is watched, and
//! its monitor may hand it to a new runner when a poll blocks; the nested
//! future is then polled under its own runtime, with no swap, and shares the
//! worker's wheels.
//!
//! A frame nested on a thread's own shard inside the poll of a foreign future
//! puts that future's wheels aside for its loop (`Aside`), and a frame of a
//! runtime already nested there shares that frame's wheels (`side_for`).
//!
//! The deadlines of the wheels kept aside are not published to their
//! runtime's paused clock: this shard is no member of it.

use core::future::Future;
use core::pin::{Pin, pin};
use core::task::{Context, Poll};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Instant;

use super::Shard;
use crate::rt::context;
use crate::rt::runtime::Inner;
use crate::time::wheel::Wheel;

/// The wheels of one nested runtime, while its future is not being polled.
pub(super) struct Side {
    inner: Arc<Inner>,
    wheel: Wheel,
    precise: Wheel,
    /// Its future is being polled: the wheels in `wheel` and `precise` of the
    /// shard are this entry's, and what is here is what they displaced.
    swapped: bool,
}

/// The runtime a shard serves, for a nested `block_on` of `inner`.
enum Serving {
    Same,
    Other(Arc<Inner>),
}

/// The first bit of a side wheel's tag: below it, entries count releases.
const TAG_SHIFT: u32 = 48;

impl Shard {
    /// The runtime this shard serves, against `inner`, the one a nested
    /// `block_on` is of. A worker knows its runtime. A thread's own shard knows it
    /// from its visiting mark (`serving`): the runtime of the outermost `block_on`
    /// running it, or of the innermost foreign frame. A loop run with no mark (a
    /// blocking wait's) serves `found`, the runtime the call found.
    fn serving(&self, found: Option<&Arc<Inner>>, inner: &Arc<Inner>) -> Serving {
        let addr = self.shared.runtime_addr.load(Ordering::Acquire);
        if addr != 0 {
            if addr == Arc::as_ptr(inner) as usize {
                return Serving::Same;
            }
            if let Some(served) = self.shared.runtime.get().and_then(Weak::upgrade) {
                return Serving::Other(served);
            }
        }
        let serving = self.serving.get();
        if !serving.is_null() {
            if core::ptr::eq(serving, Arc::as_ptr(inner)) {
                return Serving::Same;
            }
            // SAFETY: set by the foreign frame this call is nested in, which holds an
            // `&Arc<Inner>` of it for its whole run and clears it as it ends.
            return Serving::Other(unsafe {
                Arc::increment_strong_count(serving);
                Arc::from_raw(serving)
            });
        }
        match found {
            Some(found) if !Arc::ptr_eq(found, inner) => Serving::Other(Arc::clone(found)),
            _ => Serving::Same,
        }
    }

    /// Whether this shard's loop serves a runtime other than `inner`: a task `inner` spawns here
    /// is then a visitor ([`Visiting`]).
    pub(crate) fn serves_other(&self, inner: &Inner) -> bool {
        let addr = self.shared.runtime_addr.load(Ordering::Acquire);
        if addr != 0 {
            return addr != core::ptr::from_ref(inner) as usize;
        }
        let serving = self.serving.get();
        !serving.is_null() && !core::ptr::eq(serving, inner)
    }

    /// Marks `inner`, the runtime of the outermost `block_on` running this thread's own
    /// shard, as the one the shard serves, until the guard drops.
    pub(crate) fn serve<'a>(&'a self, inner: &Arc<Inner>) -> ServingSet<'a> {
        ServingSet::enter(self, inner)
    }

    /// Runs `future`, of runtime `inner` that the call found as `found`, to its end
    /// nested on this shard, under the runtime the shard serves.
    #[inline(never)]
    pub(crate) fn block_on_nested<F: Future>(
        &self,
        future: F,
        found: Option<&Arc<Inner>>,
        inner: &Arc<Inner>,
    ) -> F::Output {
        let _aside = Aside::enter(self);
        match self.serving(found, inner) {
            Serving::Same => self.block_on(future),
            Serving::Other(served) => self.block_on_foreign(future, &served, inner),
        }
    }

    /// Runs `future`, of runtime `inner`, to its end on this shard, which
    /// `outer`'s `block_on` serves.
    pub(crate) fn block_on_foreign<F: Future>(
        &self,
        future: F,
        outer: &Arc<Inner>,
        inner: &Arc<Inner>,
    ) -> F::Output {
        // A watched shard may be taken in a poll: its wheels stay in place.
        let side = (!context::watched()).then(|| self.side_for(inner));
        let at = side.map(|(at, _)| at);
        let _frame = side.map(|(_, made)| Frame { shard: self, made });
        // The loop belongs to the runtime the shard serves.
        let _outer = context::set_runtime(outer);
        let _serving = ServingSet::enter(self, outer);
        let mut future = pin!(future);
        self.block_on(core::future::poll_fn(|cx| {
            self.poll_inside(at, inner, future.as_mut(), cx)
        }))
    }

    /// The wheels for a nested frame of `inner`: those of a frame of it still
    /// nested (not made), else new ones (made).
    fn side_for(&self, inner: &Arc<Inner>) -> (usize, bool) {
        let shared = self
            .side
            .borrow()
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.inner, inner));
        match shared {
            Some(at) => (at, false),
            None => (self.push_side(inner), true),
        }
    }

    /// Makes the wheels of `inner` for a nested `block_on`, under its clock.
    fn push_side(&self, inner: &Arc<Inner>) -> usize {
        let seq = self.side_seq.get() + 1;
        self.side_seq.set(seq);
        let tag = seq << TAG_SHIFT;
        let mut wheel = Wheel::tagged(tag);
        let mut precise = Wheel::tagged(tag);
        let mut none = Vec::new();
        wheel.advance(self.wheel_now(), &mut none);
        precise.advance(self.precise_now(), &mut none);
        let mut side = self.side.borrow_mut();
        side.push(Side {
            inner: Arc::clone(inner),
            wheel,
            precise,
            swapped: false,
        });
        side.len() - 1
    }

    /// One poll of the nested future under its own runtime and wheels.
    fn poll_inside<F: Future>(
        &self,
        at: Option<usize>,
        inner: &Arc<Inner>,
        future: Pin<&mut F>,
        cx: &mut Context<'_>,
    ) -> Poll<F::Output> {
        let _runtime = context::set_runtime(inner);
        let _swapped = at.map(|at| Swapped::enter(self, at));
        future.poll(cx)
    }

    /// Swaps the wheels in place with the ones of `side[at]`.
    fn swap_side(&self, at: usize) {
        let mut side = self.side.borrow_mut();
        let entry = &mut side[at];
        core::mem::swap(&mut *self.wheel.borrow_mut(), &mut entry.wheel);
        core::mem::swap(&mut *self.precise.borrow_mut(), &mut entry.precise);
        entry.swapped = !entry.swapped;
        self.foreign_poll.set(entry.swapped);
        drop(side);
        self.count_side();
    }

    /// Makes the precise wheel in place count the timers of the wheels kept
    /// aside, so the timer pass looks at them.
    fn count_side(&self) {
        let n = self
            .side
            .borrow()
            .iter()
            .filter(|entry| !entry.swapped)
            .map(|entry| entry.wheel.armed() + entry.precise.armed())
            .sum();
        self.precise.borrow_mut().set_phantom(n);
    }

    /// Fires the wheels kept aside by their runtime's clock.
    #[cold]
    #[inline(never)]
    pub(super) fn fire_side(&self) {
        let mut fired = self.fired.take();
        {
            let mut side = self.side.borrow_mut();
            for entry in side.iter_mut().filter(|entry| !entry.swapped) {
                let _runtime = context::set_runtime(&entry.inner);
                entry.wheel.advance(self.wheel_now(), &mut fired);
                entry.precise.advance(self.precise_now(), &mut fired);
            }
        }
        self.count_side();
        self.wake_fired(fired);
    }

    /// When the loop must look at the wheels kept aside, on the loop's clock:
    /// a runtime whose clock is paused is moved by calls, not waited for.
    pub(super) fn side_deadline(&self) -> Option<Instant> {
        let side = self.side.borrow();
        let wait = side
            .iter()
            .filter(|entry| !entry.swapped && !entry.inner.clock.is_paused())
            .filter_map(|entry| {
                let coarse = entry.wheel.next_deadline().map(|ms| ms * 1_000_000);
                let fine = entry.precise.next_deadline().map(|us| us * 1_000);
                let at = coarse.into_iter().chain(fine).min()?;
                Some(at.saturating_sub(entry.inner.clock.now_nanos()))
            })
            .min()?;
        Some(crate::time::clock::now() + core::time::Duration::from_nanos(wait))
    }
}

/// Swaps a runtime's wheels in for one poll, and back, an unwind included.
struct Swapped<'a> {
    shard: &'a Shard,
    at: usize,
}

impl<'a> Swapped<'a> {
    fn enter(shard: &'a Shard, at: usize) -> Self {
        shard.swap_side(at);
        Self { shard, at }
    }
}

impl Drop for Swapped<'_> {
    fn drop(&mut self) {
        // A taken shard is another runner's: never touched again.
        if !context::detached_from(&self.shard.shared) {
            self.shard.swap_side(self.at);
        }
    }
}

/// The runtime an outermost `block_on`, or a foreign frame, runs its loop under is what its
/// shard serves, until it ends.
pub(crate) struct ServingSet<'a> {
    shard: &'a Shard,
    previous: *const Inner,
}

impl<'a> ServingSet<'a> {
    fn enter(shard: &'a Shard, outer: &Arc<Inner>) -> Self {
        let previous = shard.serving.replace(Arc::as_ptr(outer));
        Self { shard, previous }
    }
}

impl Drop for ServingSet<'_> {
    fn drop(&mut self) {
        self.shard.serving.set(self.previous);
    }
}

/// Puts the wheels of a future being polled aside, for the loop of a `block_on`
/// nested in its poll on this thread's own shard, and back after.
struct Aside<'a> {
    shard: &'a Shard,
    at: Option<usize>,
}

impl<'a> Aside<'a> {
    fn enter(shard: &'a Shard) -> Self {
        let at = shard
            .foreign_poll
            .get()
            .then(|| shard.side.borrow().iter().rposition(|entry| entry.swapped))
            .flatten();
        if let Some(at) = at {
            shard.swap_side(at);
        }
        Self { shard, at }
    }
}

impl Drop for Aside<'_> {
    fn drop(&mut self) {
        // A taken shard is another runner's: never touched again.
        if let Some(at) = self.at
            && !context::detached_from(&self.shard.shared)
        {
            self.shard.swap_side(at);
        }
    }
}

/// A nested `block_on` of another runtime is on this shard: the wheels it made
/// go when it ends.
struct Frame<'a> {
    shard: &'a Shard,
    made: bool,
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        if !self.made || context::detached_from(&self.shard.shared) {
            return;
        }
        let gone = self.shard.side.borrow_mut().pop();
        self.shard.count_side();
        // Dropped outside the borrow: the wakers it holds are user code.
        drop(gone);
    }
}

/// A task that a runtime with no worker spawned onto a shard whose loop serves another runtime
/// (a visitor): each poll, and its drop, runs with its own runtime current, so its spawns,
/// clock reads and blocking jobs go to it (`proofs/tla/nested/Nested.tla`, `TaskSeesOwn`).
/// Its sleeps wait in the wheels in place, the loop's (`W_visitor_wheels`).
pub(crate) struct Visiting<F> {
    inner: Arc<Inner>,
    future: core::mem::ManuallyDrop<F>,
}

impl<F> Visiting<F> {
    pub(crate) fn new(inner: Arc<Inner>, future: F) -> Self {
        Self {
            inner,
            future: core::mem::ManuallyDrop::new(future),
        }
    }
}

impl<F> Future for Visiting<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: `future` is never moved out of its place; `drop` drops it there.
        let this = unsafe { self.get_unchecked_mut() };
        let _visiting = context::set_runtime(&this.inner);
        // SAFETY: as above, the place is pinned with `self`.
        unsafe { Pin::new_unchecked(&mut *this.future) }.poll(cx)
    }
}

impl<F> Drop for Visiting<F> {
    fn drop(&mut self) {
        let _visiting = context::try_set_runtime(&self.inner);
        // SAFETY: dropped once, here, in its place.
        unsafe { core::mem::ManuallyDrop::drop(&mut self.future) };
    }
}
