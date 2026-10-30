//! What the runtime is doing: a snapshot of every shard's counters, taken
//! by [`stats()`](crate::stats()).
//!
//! The snapshot covers every shard alive in the process: each runtime's
//! workers, and the shard of each thread that runs `block_on` (made on its
//! first need, gone with the thread). A shard's counts are read without
//! stopping it, one at a time, so they need not agree with each other to
//! the operation. Most are counted in the shard's own memory and published
//! for snapshots: by a shard about to wait for work, so a quiet shard's are
//! current, and by a busy one at least once every few hundred task runs.
//! Errors are counted as they happen. Taking a snapshot costs a read of each shard's
//! counters; a running shard pays nothing for it.

use std::io;

use crate::IoBackend;
use crate::rt::counters::{KINDS, kind_index};

/// A snapshot of the runtime's counters: [`stats()`](crate::stats()).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Stats {
    /// Every shard alive: the workers first, by their place in their
    /// runtime, then the shards of threads that run `block_on`.
    pub shards: Vec<ShardStats>,
    /// Threads of the blocking pools alive now, at work or idle.
    pub blocking_threads: usize,
    /// Errors of IO run on threads that run no shard: a handle polled
    /// outside nuclei's `block_on` and its tasks, or a completion delivered
    /// while its shard was not running.
    pub off_shard_errors: ErrorCounts,
}

/// One shard's counters. Those marked "published" are as the shard last
/// published them (see [the module](self)).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ShardStats {
    /// The shard's place among its runtime's workers; `None` for the shard
    /// of a thread that runs `block_on`.
    pub worker: Option<usize>,
    /// The kernel interface the shard's IO runs on.
    pub backend: IoBackend,
    /// Tasks placed on the shard and not done yet.
    pub tasks: usize,
    /// Task polls the shard has run (published).
    pub polls: u64,
    /// Times the shard slept with nothing to run (published).
    pub parks: u64,
    /// Time the shard spent asleep, in all (published).
    pub parked: std::time::Duration,
    /// Time since the shard was made that it did not spend asleep: running
    /// tasks, driving its IO and timers, spinning before a park (from the
    /// published time asleep).
    pub busy: std::time::Duration,
    /// Runnables waiting in the shard's own queue, its LIFO slot included
    /// (published).
    pub queue_depth: usize,
    /// Runnables of the shard's tasks it took from other threads: a task
    /// woken there, or spawned onto it from there (published).
    pub remote_wakes: u64,
    /// IO operations the shard handed to the kernel that have not ended:
    /// on io_uring the operations submitted, on IOCP the ones issued. A
    /// readiness poller (epoll, kqueue) hands the kernel none, as its
    /// driver makes each call once the descriptor is ready: 0 there
    /// (published).
    pub ops_in_flight: usize,
    /// Times the shard found completions the kernel was holding for want of
    /// room in the ring's completion queue (none is lost: the kernel keeps
    /// them until there is room); 0 on other backends (published).
    pub cq_overflows: u64,
    /// Bytes the shard's IO buffers hold: its pool's, in use and free, and on
    /// io_uring its receive buffers' (published).
    pub buffer_bytes: usize,
    /// The most bytes the shard's IO buffers may hold, its receive buffers
    /// included ([`Builder::io_buffer_cap`](crate::Builder::io_buffer_cap)).
    pub buffer_cap: usize,
    /// Times a buffer was wanted while every byte under the pool's cap was
    /// in use; the IO waits for one to come back (published).
    pub buffer_exhausted: u64,
    /// Timers armed on the shard (published).
    pub timers: usize,
    /// Times the shard went to another thread because the thread running it
    /// stayed in one poll past [`Builder::handoff_after`](crate::Builder::handoff_after).
    pub handoffs: u64,
    /// The longest poll the runtime saw on the shard, at the resolution of
    /// its watch (a quarter of the handoff threshold); zero when no poll
    /// lasted that long, or with the handoff off.
    pub longest_poll: std::time::Duration,
    /// Errors of IO run on the shard.
    pub errors: ErrorCounts,
}

/// IO errors counted by [`io::ErrorKind`], and two kinds of failure
/// counted apart.
///
/// An error counted is one an operation on a socket, pipe or file ended
/// with, as the system gave it: not a "try again" the driver answers by
/// waiting, nor the cancel of an operation whose task went away.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ErrorCounts {
    pub(crate) by_kind: [u64; KINDS.len()],
    /// Handles the shard could not register: the kernel refused to watch
    /// the descriptor, or to let the shard's driver take it.
    pub refused_registrations: u64,
    /// Accepts that failed because the process or the system had no file
    /// descriptor left (counted among the errors by kind too).
    pub accepts_out_of_descriptors: u64,
}

impl ErrorCounts {
    /// Errors of `kind`. The kinds are counted apart from a fixed table
    /// holding every stable kind the system's IO errors map to; one outside
    /// it is counted, and read, as [`io::ErrorKind::Other`].
    pub fn get(&self, kind: io::ErrorKind) -> u64 {
        self.by_kind[kind_index(kind)]
    }

    /// Each kind with errors, and how many, in the table's fixed order.
    pub fn iter(&self) -> impl Iterator<Item = (io::ErrorKind, u64)> + '_ {
        KINDS
            .iter()
            .copied()
            .zip(self.by_kind.iter().copied())
            .filter(|&(_, n)| n > 0)
    }
}

impl Default for ErrorCounts {
    fn default() -> Self {
        Self {
            by_kind: [0; KINDS.len()],
            refused_registrations: 0,
            accepts_out_of_descriptors: 0,
        }
    }
}

impl core::fmt::Debug for ErrorCounts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ErrorCounts")
            .field("by_kind", &self.iter().collect::<Vec<_>>())
            .field("refused_registrations", &self.refused_registrations)
            .field(
                "accepts_out_of_descriptors",
                &self.accepts_out_of_descriptors,
            )
            .finish()
    }
}

/// A snapshot of the counters of every shard alive in the process and of
/// the blocking pools. See [`Stats`].
///
/// ```
/// let stats = nuclei::stats();
/// for shard in &stats.shards {
///     println!("{:?}: {} tasks, {} polls", shard.worker, shard.tasks, shard.polls);
/// }
/// ```
pub fn stats() -> Stats {
    crate::rt::counters::snapshot()
}
