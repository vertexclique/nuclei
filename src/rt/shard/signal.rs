//! The waker of a shard's `block_on` future.

use std::sync::Arc;
use std::task::Wake;

use super::Shared;
use crate::sync::internal::{AtomicBool, Ordering};

/// The waker of a `block_on` future: marks it woken and wakes the shard.
pub(super) struct Signal {
    pub(super) woken: AtomicBool,
    pub(super) shared: Arc<Shared>,
}

impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        self.shared.unpark();
    }
}
