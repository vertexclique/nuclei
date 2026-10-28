//! A runner's slot: the word the runner writes as it leaves its shard for a
//! task's code and comes back, and the claim the monitor writes.
//!
//! The word is `epoch << 1 | outside`: the runner's poll of epoch `e` is
//! `outside(e)` while the task's code runs, `inside(e)` while the runtime's
//! does. Only the runner writes it. The claim is [`NONE`], [`PENDING`] (the
//! monitor is about to take the shard) or [`TAKEN`]; the monitor sets it
//! pending and takes with a compare-and-swap from pending to taken, and the
//! runner, coming back while it is pending, cancels it with a
//! compare-and-swap from pending to none: one of the two wins, and neither
//! side waits for the other (proofs/tla/handoff, `Exclusive`).
//!
//! The runner's way back in is a store of the word, a fence and a load of the
//! claim. Where the monitor has a heavy fence (one that orders every thread
//! of the process, [`super::fence`]), the runner's fence is the compiler's
//! only; elsewhere it is a full one.

use core::sync::atomic::compiler_fence;

use crate::sync::internal::{AtomicU8, AtomicU64, Ordering, fence};

/// No claim: the shard is its runner's.
pub(crate) const NONE: u8 = 0;
/// The monitor is about to take the shard.
pub(crate) const PENDING: u8 = 1;
/// The monitor took the shard: its runner touches it no more.
pub(crate) const TAKEN: u8 = 2;

/// The word of a runner in the poll of epoch `epoch`, outside the shard.
pub(crate) const fn outside(epoch: u64) -> u64 {
    (epoch << 1) | 1
}

/// The word of a runner in the poll of epoch `epoch`, inside the shard.
pub(crate) const fn inside(epoch: u64) -> u64 {
    epoch << 1
}

/// Whether `word` says its runner is outside the shard (in a task's code).
pub(crate) const fn is_outside(word: u64) -> bool {
    word & 1 == 1
}

/// The epoch `word` names.
#[cfg(test)]
pub(crate) const fn epoch(word: u64) -> u64 {
    word >> 1
}

/// What a runner coming back into its shard finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Back {
    /// The shard is still its own: it went in.
    In,
    /// The monitor took the shard: the runner is detached from it.
    Taken,
}

/// A runner's slot. A slot is never handed to another runner: a late write
/// of a runner detached from its shard lands in its own slot.
pub(crate) struct Slot {
    word: AtomicU64,
    claim: AtomicU8,
    /// The monitor has no heavy fence: the runner's fence is a full one.
    full_fence: bool,
}

impl Slot {
    /// A slot for a runner, in the loop of epoch 0; `full_fence` when the
    /// monitor has no heavy fence (see [`super::fence`]).
    pub(crate) fn new(full_fence: bool) -> Self {
        Self {
            word: AtomicU64::new(inside(0)),
            claim: AtomicU8::new(NONE),
            full_fence,
        }
    }

    /// The runner leaves the shard for a task's code, in the poll of
    /// `epoch`. Release: whoever takes the shard on reading this word sees
    /// every write the runner made to it before.
    #[inline]
    pub(crate) fn go_out(&self, epoch: u64) {
        self.word.store(outside(epoch), Ordering::Release);
    }

    /// The runner comes back into the shard from the poll of `epoch`: in,
    /// unless the monitor took the shard. Never waits.
    #[inline]
    pub(crate) fn come_back(&self, epoch: u64) -> Back {
        self.word.store(inside(epoch), Ordering::Relaxed);
        if self.full_fence {
            fence(Ordering::SeqCst);
        } else {
            // The monitor's heavy fence orders this thread's store before
            // its load, as seen from the monitor; the compiler must not.
            compiler_fence(Ordering::SeqCst);
        }
        if self.claim.load(Ordering::Relaxed) == NONE {
            return Back::In;
        }
        self.contest()
    }

    /// A claim is up: a pending one is cancelled, a taken one detaches.
    #[cold]
    #[inline(never)]
    fn contest(&self) -> Back {
        match self
            .claim
            .compare_exchange(PENDING, NONE, Ordering::Acquire, Ordering::Acquire)
        {
            Ok(_) => Back::In,
            Err(TAKEN) => Back::Taken,
            // The monitor cancelled meanwhile.
            Err(_) => Back::In,
        }
    }

    /// The runner's word as the monitor reads it. Acquire: pairs with
    /// [`go_out`](Self::go_out).
    pub(crate) fn word(&self) -> u64 {
        self.word.load(Ordering::Acquire)
    }

    /// The monitor's claim, before its heavy fence.
    pub(crate) fn claim(&self) {
        self.claim.store(PENDING, Ordering::Relaxed);
    }

    /// The monitor's take after its heavy fence found the runner outside:
    /// whether it took the shard (the runner did not cancel first).
    /// Release: the runner's detach sees the claim's change.
    pub(crate) fn take(&self) -> bool {
        self.claim
            .compare_exchange(PENDING, TAKEN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The monitor's cancel: the runner was not outside after the fence.
    pub(crate) fn cancel(&self) {
        let _ = self
            .claim
            .compare_exchange(PENDING, NONE, Ordering::AcqRel, Ordering::Relaxed);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn the_word_packs_its_epoch_and_side_and_unpacks_them() {
        for e in [0, 1, 2, 41, u64::MAX >> 1] {
            assert!(is_outside(outside(e)));
            assert!(!is_outside(inside(e)));
            assert_eq!(epoch(outside(e)), e);
            assert_eq!(epoch(inside(e)), e);
            assert_ne!(outside(e), inside(e));
        }
        assert_ne!(outside(1), outside(2), "a new poll changes the word");
    }

    #[test]
    fn a_runner_with_no_claim_goes_back_in() {
        for full in [false, true] {
            let slot = Slot::new(full);
            slot.go_out(1);
            assert_eq!(slot.word(), outside(1));
            assert_eq!(slot.come_back(1), Back::In);
            assert_eq!(slot.word(), inside(1));
        }
    }

    #[test]
    fn a_runner_back_first_cancels_the_claim_and_the_take_then_fails() {
        let slot = Slot::new(false);
        slot.go_out(3);
        slot.claim();
        assert_eq!(slot.come_back(3), Back::In);
        assert!(!slot.take(), "the runner cancelled first");
        assert_eq!(slot.come_back(3), Back::In);
    }

    #[test]
    fn a_take_before_the_runner_is_back_detaches_it() {
        let slot = Slot::new(false);
        slot.go_out(5);
        slot.claim();
        assert!(slot.take());
        assert_eq!(slot.come_back(5), Back::Taken);
        assert_eq!(slot.come_back(5), Back::Taken, "for good");
    }

    #[test]
    fn a_cancel_by_the_monitor_lets_the_runner_back_in() {
        let slot = Slot::new(true);
        slot.go_out(7);
        slot.claim();
        slot.cancel();
        assert_eq!(slot.come_back(7), Back::In);
        assert!(!slot.take(), "nothing pending");
    }
}
