//! A caller's clear of a side's readiness that found the side held ([`LeftClear`]): its tick
//! is left for the holder, who applies it as it lets go (proofs/tla/asyncfd/AsyncFd.tla, the
//! `drop_held_clear` and `clear_last_wins` mutations).

use core::sync::atomic::{AtomicU64, Ordering};

/// Set beside the tick while one is left.
const LEFT: u64 = 1 << 32;

/// The tick of a clear left for a side's holder: of several left before the holder took them,
/// the newest, the only one that can still clear the side (an older one is of a look an edge
/// came after). Empty once taken.
#[derive(Default)]
pub(super) struct LeftClear(AtomicU64);

impl LeftClear {
    /// Leaves `tick`, unless a newer one is left already.
    pub(super) fn leave(&self, tick: u32) {
        // Release: the holder that takes the side's deferred bit, left after this, reads it.
        let _ = self
            .0
            .try_update(Ordering::Release, Ordering::Relaxed, |left| {
                (left & LEFT == 0 || newer(tick, left as u32)).then_some(LEFT | u64::from(tick))
            });
    }

    /// The tick left, if one is, taken.
    pub(super) fn take(&self) -> Option<u32> {
        // Acquire: pairs with the Release of `leave`.
        let left = self.0.swap(0, Ordering::Acquire);
        (left & LEFT != 0).then_some(left as u32)
    }
}

/// Whether edge count `a` comes after `b`: the counts wrap, and every tick left at once is of a
/// look within a few edges of the others.
fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}
