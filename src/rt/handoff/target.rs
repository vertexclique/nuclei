//! A wake target that moves once: the shard a `block_on` future, or a
//! runner's bound tasks, are woken on, moved from a worker shard taken from
//! its thread to that thread's own shard.
//!
//! The waker writes what it has to say (the woken flag, a push) and then
//! reads the target; the mover sets the target and then looks for what was
//! said before it sleeps on the new one. Both pairs are sequentially
//! consistent, so either the mover finds the flag or the push, or the waker
//! wakes the new shard (proofs/tla/handoff, `Retarget`).

use std::sync::{Arc, OnceLock};

use crate::rt::shard::Shared;
use crate::sync::internal::{AtomicBool, Ordering};

/// A shard to wake, which may move once.
pub(crate) struct Target {
    first: Arc<Shared>,
    then: OnceLock<Arc<Shared>>,
    moved: AtomicBool,
}

impl Target {
    /// Wakes on `first` until moved.
    pub(crate) fn new(first: Arc<Shared>) -> Self {
        Self {
            first,
            then: OnceLock::new(),
            moved: AtomicBool::new(false),
        }
    }

    /// The shard to wake now, read after the caller's sequentially consistent
    /// write of what it has to say.
    pub(crate) fn get(&self) -> &Arc<Shared> {
        if self.moved.load(Ordering::SeqCst) {
            // Set before `moved` rose.
            self.then.get().unwrap_or(&self.first)
        } else {
            &self.first
        }
    }

    /// Whether the target moved.
    pub(crate) fn moved(&self) -> bool {
        self.moved.load(Ordering::Acquire)
    }

    /// Moves the target to `to`; a second move does nothing. The caller then
    /// looks for what was said before it sleeps on `to`.
    pub(crate) fn move_to(&self, to: &Arc<Shared>) {
        if self.then.set(Arc::clone(to)).is_ok() {
            self.moved.store(true, Ordering::SeqCst);
        }
    }
}
