//! A snapshot of one runtime's counters: [`Runtime::stats`] and
//! [`RuntimeHandle::stats`].

use super::{Inner, Runtime, RuntimeHandle};
use crate::stats::Stats;

impl Inner {
    /// The counts of this runtime's workers, by place, and of its blocking
    /// pool.
    fn stats(&self) -> Stats {
        crate::rt::counters::runtime_snapshot(
            self.workers.shards().map(|shard| shard.stats()),
            self.pool.threads_now(),
            self.pool.queued(),
        )
    }
}

impl Runtime {
    /// A snapshot of this runtime's counters: [`Stats::shards`] lists its
    /// workers (stopped ones included) by place, and the blocking counts are
    /// its pool's. The errors of threads that run no shard and the receive
    /// buffers missing are the process's, as in [`stats()`](crate::stats()):
    /// they belong to no runtime. The shard of a thread inside this
    /// runtime's `block_on` serves every runtime the thread enters, so only
    /// [`stats()`](crate::stats()) lists it.
    pub fn stats(&self) -> Stats {
        self.inner.stats()
    }
}

impl RuntimeHandle {
    /// A snapshot of the counters of the handle's runtime; see
    /// [`Runtime::stats`].
    pub fn stats(&self) -> Stats {
        self.inner.stats()
    }
}
