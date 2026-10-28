//! The waker of a shard's `block_on` future.

use std::sync::Arc;
use std::task::Wake;

use super::Shared;
use crate::rt::handoff::target::Target;
use crate::sync::internal::{AtomicBool, Ordering};

/// The waker of a `block_on` future: marks it woken and wakes the shard it
/// runs on, which moves when its shard is taken from its thread (see
/// `shard::run`).
pub(crate) struct Signal {
    pub(super) woken: AtomicBool,
    pub(super) target: Target,
}

impl Signal {
    /// A future's waker on the shard of `shared`, woken once so its first
    /// poll comes at once.
    pub(super) fn new(shared: &Arc<Shared>) -> Self {
        Self {
            woken: AtomicBool::new(true),
            target: Target::new(Arc::clone(shared)),
        }
    }
}

impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Sequentially consistent, as the target's read: either the thread
        // moving the future to its own shard finds the flag, or this wakes
        // that shard.
        self.woken.store(true, Ordering::SeqCst);
        self.target.get().unpark();
    }
}
