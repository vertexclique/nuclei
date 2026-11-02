//! The threads whose poll lost a worker's shard to a handoff and that still run what they ran of
//! it (the rest of the poll, then their bound tasks): each counts among the shard's live work
//! until it is done, so a stopped worker stays up to lend the shard back to its calls
//! (`rt::handoff::loan`, proofs/tla/handoff/HandBack.tla, `M_drain_early`).

use std::sync::Arc;

use super::{Shard, Shared};
use crate::rt::handoff::Monitor;
use crate::sync::internal::Ordering;

impl Shared {
    /// A thread whose poll lost the shard runs what it ran of it on: a stopped worker stays up
    /// until it is done. Its poll's task is live meanwhile, so the worker has not exited.
    pub(crate) fn detached_began(&self) {
        self.detached.fetch_add(1, Ordering::Relaxed);
    }

    /// The thread of [`detached_began`](Self::detached_began) is done with the shard.
    pub(crate) fn detached_ended(&self) {
        if self.detached.fetch_sub(1, Ordering::Release) == 1 {
            self.last_task_done();
        }
    }

    /// No live task, and no thread that lost the shard running what it ran of it, read after a
    /// sequentially consistent fence: what a draining worker exits on.
    pub(super) fn nothing_live(&self) -> bool {
        self.load.load(Ordering::Acquire) == 0 && self.detached.load(Ordering::Acquire) == 0
    }
}

impl Shard {
    /// The monitor that watches this shard, a worker's with the handoff on. Fixed at the
    /// shard's making, so any thread that holds the shard may read it.
    pub(crate) fn monitor(&self) -> Option<&Arc<Monitor>> {
        self.monitor.as_ref()
    }
}
