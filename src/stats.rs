//! What the runtime is doing: a snapshot of every shard's counters, taken
//! by [`stats()`](crate::stats()), or of one runtime's by
//! [`Runtime::stats`](crate::Runtime::stats). A runtime built with
//! [`Builder::record_latencies`](crate::Builder::record_latencies) adds a
//! [`Histogram`] of its poll times, and with the `io-stats` feature each
//! worker's IO counts and latencies.
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
    /// Jobs waiting in the blocking pools' queues for a thread to take them.
    pub blocking_queue_depth: usize,
    /// Errors of IO run on threads that run no shard: a handle polled
    /// outside nuclei's `block_on` and its tasks, or a completion delivered
    /// while its shard was not running.
    pub off_shard_errors: ErrorCounts,
    /// Shards that started without the io_uring receive buffers their
    /// configuration asked for, because the system had no memory for them
    /// (most often the locked-memory limit); their reads use the pool's
    /// buffers instead.
    pub receive_buffers_missing: u64,
    /// Files whose bytes written behind were given up with no caller left
    /// to answer: a dropped handle's write that failed or whose
    /// [`write_behind_linger`](crate::runtime::Builder::write_behind_linger)
    /// ran out, one handed back by `get_mut` or `into_inner` the same way, or
    /// one still out when the process exited. Each is also said once at
    /// warn.
    pub write_behind_lost: u64,
}

/// One shard's counters. Those marked "published" are as the shard last
/// published them (see [the module](self)).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ShardStats {
    /// The shard's place among its runtime's workers; `None` for the shard
    /// of a thread that runs `block_on`.
    pub worker: Option<usize>,
    /// The runtime the shard is a worker of; `None` for the shard of a
    /// thread that runs `block_on`, which serves every runtime the thread
    /// enters.
    pub runtime: Option<crate::runtime::RuntimeId>,
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
    /// Times a task's poll on the shard spent its budget of operations and
    /// yielded to the shard's other tasks (see
    /// [`task::coop`](crate::task::coop)).
    pub budget_yields: u64,
    /// How long each task poll on the shard ran, while its runtime records
    /// it ([`Builder::record_latencies`](crate::Builder::record_latencies));
    /// `None` when it does not, which is not a shard with no polls.
    pub poll_times: Option<Histogram>,
    /// How long the shard's IO operations took from their submission to the
    /// kernel to their completion, by kind, while its runtime records
    /// latencies ([`Builder::record_latencies`](crate::Builder::record_latencies));
    /// `None` when it does not, and `None` on a readiness backend (epoll,
    /// kqueue), whose calls are made once the kernel reports the descriptor
    /// ready, so there is no wait in the kernel per operation to time. With
    /// the `io-stats` feature only.
    #[cfg(feature = "io-stats")]
    pub io_latencies: Option<IoLatencies>,
    /// The IO operations the shard's thread completed, while its runtime
    /// counts them ([`Builder::count_io`](crate::Builder::count_io));
    /// `None` when it does not, which is not a shard that did no IO
    /// (published). With the `io-stats` feature only.
    #[cfg(feature = "io-stats")]
    pub io: Option<IoCounts>,
    /// Errors of IO run on the shard.
    pub errors: ErrorCounts,
}

/// The IO operations a shard's thread completed: see [`ShardStats::io`].
/// With the `io-stats` feature only.
///
/// An operation is counted when the kernel completes it: a read or receive
/// that returned bytes or the end of the stream, a write or send that took
/// bytes, a connection accepted. A read served from the read-ahead is
/// counted when the read-ahead's own read completed, and a write taken into
/// the write-behind when the write-behind's own write did. Peeks and
/// readiness probes are not counted. IO completed on a thread that runs no
/// shard (the blocking pool's own calls, a thread outside the runtime) is
/// counted by no shard.
#[cfg(feature = "io-stats")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct IoCounts {
    /// Reads and receives.
    pub reads: u64,
    /// Writes and sends.
    pub writes: u64,
    /// Bytes the counted reads returned.
    pub bytes_read: u64,
    /// Bytes the counted writes took.
    pub bytes_written: u64,
    /// Connections accepted.
    pub accepts: u64,
}

/// How long each kind of a shard's IO operations took, from their
/// submission to the kernel to their completion: see
/// [`ShardStats::io_latencies`].
///
/// A multishot receive or accept is one operation for many arrivals and is
/// not timed; nor is a cancelled operation or a readiness probe. File calls
/// are timed on the ring, not the ones the blocking pool makes.
#[cfg(feature = "io-stats")]
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct IoLatencies {
    /// Reads and receives.
    pub reads: Histogram,
    /// Writes and sends.
    pub writes: Histogram,
    /// Accepts, one connection each.
    pub accepts: Histogram,
    /// Connects.
    pub connects: Histogram,
    /// Reads and writes of files.
    pub files: Histogram,
}

/// Buckets of a [`Histogram`].
pub(crate) const BUCKETS: usize = 64;

/// The bucket of a sample of `nanos` nanoseconds.
pub(crate) fn bucket_of(nanos: u64) -> usize {
    (u64::BITS - 1 - (nanos | 1).leading_zeros()) as usize
}

/// A log2 histogram of durations: bucket `i` counts the samples from `2^i`
/// nanoseconds up to (and not including) `2^(i+1)`; the first bucket also
/// counts samples under a nanosecond.
#[derive(Clone, PartialEq, Eq)]
pub struct Histogram {
    pub(crate) buckets: [u64; BUCKETS],
}

impl Histogram {
    /// The samples counted.
    pub fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// Each bucket that holds samples: its lowest duration, the duration it
    /// ends before, and its count, from the shortest up.
    pub fn buckets(
        &self,
    ) -> impl Iterator<Item = (std::time::Duration, std::time::Duration, u64)> + '_ {
        self.buckets
            .iter()
            .enumerate()
            .filter(|&(_, &n)| n > 0)
            .map(|(i, &n)| {
                let low = if i == 0 { 0 } else { 1u64 << i };
                let high = 1u64.checked_shl(i as u32 + 1).unwrap_or(u64::MAX);
                (
                    std::time::Duration::from_nanos(low),
                    std::time::Duration::from_nanos(high),
                    n,
                )
            })
    }
}

impl core::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.buckets()).finish()
    }
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
