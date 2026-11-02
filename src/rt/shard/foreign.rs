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
//! The deadlines of the wheels kept aside are not published to their
//! runtime's paused clock: this shard is no member of it.

use core::future::Future;
use core::pin::{Pin, pin};
use core::task::{Context, Poll};
use std::sync::Arc;
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

/// The first bit of a side wheel's tag: below it, entries count releases.
const TAG_SHIFT: u32 = 48;

impl Shard {
    /// Runs `future`, of runtime `inner`, to its end on this shard, which
    /// `outer`'s `block_on` serves.
    pub(crate) fn block_on_foreign<F: Future>(
        &self,
        future: F,
        outer: &Arc<Inner>,
        inner: &Arc<Inner>,
    ) -> F::Output {
        // A watched shard may be taken in a poll: its wheels stay in place.
        let at = (!context::watched()).then(|| self.push_side(inner));
        let _frame = at.map(|_| Frame(self));
        // The loop belongs to the runtime the shard serves.
        let _outer = context::set_runtime(outer);
        let mut future = pin!(future);
        self.block_on(core::future::poll_fn(|cx| {
            self.poll_inside(at, inner, future.as_mut(), cx)
        }))
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

/// A nested `block_on` of another runtime is on this shard: its wheels go
/// when it ends.
struct Frame<'a>(&'a Shard);

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        if context::detached_from(&self.0.shared) {
            return;
        }
        let gone = self.0.side.borrow_mut().pop();
        self.0.count_side();
        // Dropped outside the borrow: the wakers it holds are user code.
        drop(gone);
    }
}
