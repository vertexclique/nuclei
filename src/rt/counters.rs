//! Each shard's counters, the registry of live shards, and the snapshot
//! [`stats`](crate::stats()) takes of them.
//!
//! A shard's [`Counters`] sit in its `Shared`, on cache lines of their own:
//! the shard writes them and other threads read them only for a snapshot,
//! so counting never moves a line another core is using. A count is one of
//! two kinds:
//!
//! - what lives in the shard's own cells (task polls, the remote runnables
//!   it took, armed timers, the buffer pool, the operations in flight,
//!   completion-queue overflows), counted there as plain cells and
//!   published as the shard looks at its driver: every time it parks, so
//!   just before it sleeps, and every
//!   [`PUBLISH_INTERVAL`](super::shard::PUBLISH_INTERVAL)th look while it is
//!   busy, which it makes at least once every
//!   [`DRIVER_INTERVAL`](super::shard::DRIVER_INTERVAL) task runs: a
//!   snapshot sees them as the shard last published them;
//! - the IO operations the shard's thread completed, counted in the thread's
//!   own cells ([`tally`]) and drained into the shard's totals as it
//!   publishes: IO on a thread that runs no shard is counted by none;
//! - errors, counted by whichever thread met them, against the shard that
//!   thread runs, or in one table for the threads that run none: an atomic
//!   add, on a path that failed already.
//!
//! The registry lists each shard from its making to its drop. A shard that
//! goes only marks itself gone: it may go in its thread's exit, after the
//! map's per-thread state went, when what the map takes out could no
//! longer be freed. Its entry is taken out at the next shard's making or
//! the next snapshot, on a thread that runs. Every count is `Relaxed`: a
//! snapshot orders nothing, and each value in it is one the counter held
//! while the snapshot was taken.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::io;
use std::sync::{Arc, OnceLock, Weak};

#[cfg(feature = "io-stats")]
pub(crate) mod tally;
// Without `io-stats`: every count compiles to nothing.
#[cfg(not(feature = "io-stats"))]
#[path = "counters/tally_off.rs"]
pub(crate) mod tally;

use super::shard::Shared;
use crate::IoBackend;
use crate::stats::{ErrorCounts, ShardStats, Stats};

/// The error kinds counted apart, in the order [`ErrorCounts::iter`]
/// gives them; a kind not here is counted as [`io::ErrorKind::Other`].
pub(crate) const KINDS: [io::ErrorKind; 39] = {
    use io::ErrorKind::*;
    [
        NotFound,
        PermissionDenied,
        ConnectionRefused,
        ConnectionReset,
        HostUnreachable,
        NetworkUnreachable,
        ConnectionAborted,
        NotConnected,
        AddrInUse,
        AddrNotAvailable,
        NetworkDown,
        BrokenPipe,
        AlreadyExists,
        WouldBlock,
        NotADirectory,
        IsADirectory,
        DirectoryNotEmpty,
        ReadOnlyFilesystem,
        StaleNetworkFileHandle,
        InvalidInput,
        InvalidData,
        TimedOut,
        WriteZero,
        StorageFull,
        NotSeekable,
        QuotaExceeded,
        FileTooLarge,
        ResourceBusy,
        ExecutableFileBusy,
        Deadlock,
        CrossesDevices,
        TooManyLinks,
        InvalidFilename,
        ArgumentListTooLong,
        Interrupted,
        Unsupported,
        UnexpectedEof,
        OutOfMemory,
        Other,
    ]
};

/// Where `kind` is counted in [`KINDS`].
pub(crate) fn kind_index(kind: io::ErrorKind) -> usize {
    // Only on a path that failed: a scan of the table costs nothing there.
    KINDS
        .iter()
        .position(|&k| k == kind)
        .unwrap_or(KINDS.len() - 1)
}

/// What a shard counts, which other threads read for a snapshot.
// Two lines of 64 bytes: some cores fetch lines in pairs, and some have
// 128-byte lines.
#[repr(align(128))]
pub(crate) struct Counters {
    polls: AtomicU64,
    parks: AtomicU64,
    parked_nanos: AtomicU64,
    queue_depth: AtomicUsize,
    /// When the shard was made: its busy time is its age less its parks.
    born: std::time::Instant,
    remote_wakes: AtomicU64,
    timers: AtomicUsize,
    buffer_bytes: AtomicUsize,
    buffer_exhausted: AtomicU64,
    ops_in_flight: AtomicUsize,
    cq_overflows: AtomicU64,
    /// Times the shard was handed to another thread, its runner blocked.
    handoffs: AtomicU64,
    /// The longest poll the runtime's monitor saw, in nanoseconds.
    longest_poll_nanos: AtomicU64,
    /// Times a task's poll spent its budget and yielded.
    budget_yields: AtomicU64,
    /// The poll times, while the runtime records them (made once, before the
    /// shard runs anything).
    poll_times: OnceLock<Box<Buckets>>,
    /// The IO latencies, while the runtime records them (made with
    /// `poll_times`); shared with the shard's ring or port.
    #[cfg(feature = "io-stats")]
    io_times: OnceLock<Arc<IoTimes>>,
    /// The IO operations counted, while the runtime counts them (made once,
    /// before the shard runs anything).
    #[cfg(feature = "io-stats")]
    io: OnceLock<Box<IoTotals>>,
    /// The shard's runtime and its place among the runtime's workers.
    owner: Option<(u64, usize)>,
    /// The shard went: its `Shared` may outlive it, held by a waker.
    gone: AtomicBool,
    backend: IoBackend,
    buffer_cap: usize,
    errors: Errors,
}

/// What a shard's own cells hold, as it publishes them.
pub(crate) struct Published {
    pub(crate) polls: u64,
    pub(crate) parks: u64,
    pub(crate) parked: std::time::Duration,
    pub(crate) queue_depth: usize,
    pub(crate) remote_wakes: u64,
    pub(crate) timers: usize,
    pub(crate) buffer_bytes: usize,
    pub(crate) buffer_exhausted: u64,
    pub(crate) ops_in_flight: usize,
    pub(crate) cq_overflows: u64,
}

impl Counters {
    /// The counters of a shard of `backend` whose pool holds `buffer_cap`
    /// bytes, worker `owner` (a runtime and a place) if it is one.
    pub(crate) fn new(backend: IoBackend, buffer_cap: usize, owner: Option<(u64, usize)>) -> Self {
        Self {
            polls: AtomicU64::new(0),
            parks: AtomicU64::new(0),
            parked_nanos: AtomicU64::new(0),
            queue_depth: AtomicUsize::new(0),
            born: std::time::Instant::now(),
            remote_wakes: AtomicU64::new(0),
            timers: AtomicUsize::new(0),
            buffer_bytes: AtomicUsize::new(0),
            buffer_exhausted: AtomicU64::new(0),
            ops_in_flight: AtomicUsize::new(0),
            cq_overflows: AtomicU64::new(0),
            handoffs: AtomicU64::new(0),
            longest_poll_nanos: AtomicU64::new(0),
            budget_yields: AtomicU64::new(0),
            poll_times: OnceLock::new(),
            #[cfg(feature = "io-stats")]
            io_times: OnceLock::new(),
            #[cfg(feature = "io-stats")]
            io: OnceLock::new(),
            owner,
            gone: AtomicBool::new(false),
            backend,
            buffer_cap,
            errors: Errors::new(),
        }
    }

    /// Publishes what the shard's own cells hold. Its own thread only.
    pub(crate) fn publish(&self, now: Published) {
        self.polls.store(now.polls, Ordering::Relaxed);
        self.parks.store(now.parks, Ordering::Relaxed);
        let parked = u64::try_from(now.parked.as_nanos()).unwrap_or(u64::MAX);
        self.parked_nanos.store(parked, Ordering::Relaxed);
        self.queue_depth.store(now.queue_depth, Ordering::Relaxed);
        self.remote_wakes.store(now.remote_wakes, Ordering::Relaxed);
        self.timers.store(now.timers, Ordering::Relaxed);
        self.buffer_bytes.store(now.buffer_bytes, Ordering::Relaxed);
        self.buffer_exhausted
            .store(now.buffer_exhausted, Ordering::Relaxed);
        self.ops_in_flight
            .store(now.ops_in_flight, Ordering::Relaxed);
        self.cq_overflows.store(now.cq_overflows, Ordering::Relaxed);
        // The thread's counts go to the shard it runs, or nowhere if that
        // shard's runtime does not count them.
        #[cfg(feature = "io-stats")]
        if tally::counting() {
            let io = tally::take();
            if let Some(totals) = self.io.get() {
                totals.add(&io);
            }
        }
    }

    /// The shard counts its IO operations from now on.
    #[cfg(feature = "io-stats")]
    pub(crate) fn count_io(&self) {
        tally::start();
        let _ = self.io.set(Box::default());
    }

    /// The shard was handed to another thread, its runner blocked.
    pub(crate) fn handed_off(&self) {
        self.handoffs.fetch_add(1, Ordering::Relaxed);
    }

    /// The monitor saw a poll of at least `length`.
    pub(crate) fn longest_poll(&self, length: std::time::Duration) {
        let nanos = u64::try_from(length.as_nanos()).unwrap_or(u64::MAX);
        self.longest_poll_nanos.fetch_max(nanos, Ordering::Relaxed);
    }

    /// The shard records the time of each poll from now on.
    pub(crate) fn record_latencies(&self) {
        let _ = self.poll_times.set(Box::new(Buckets::new()));
        #[cfg(feature = "io-stats")]
        let _ = self.io_times.set(Arc::new(IoTimes::new()));
    }

    #[cfg(all(completion, feature = "io-stats"))]
    /// The IO latency buckets, if the shard records them.
    pub(crate) fn io_times(&self) -> Option<&Arc<IoTimes>> {
        self.io_times.get()
    }

    /// A poll of `length` ran on the shard, which records poll times.
    pub(crate) fn poll_time(&self, length: std::time::Duration) {
        if let Some(buckets) = self.poll_times.get() {
            buckets.add(length);
        }
    }

    /// A poll on the shard spent its budget and yielded. Only the thread
    /// inside the shard counts it: a load and a store.
    pub(crate) fn budget_yield(&self) {
        let n = self.budget_yields.load(Ordering::Relaxed);
        self.budget_yields.store(n + 1, Ordering::Relaxed);
    }

    /// The runtime the shard is a worker of.
    pub(crate) fn runtime(&self) -> Option<u64> {
        self.owner.map(|(runtime, _)| runtime)
    }

    /// The shard went: `stats` lists it no more.
    pub(crate) fn went(&self) {
        self.gone.store(true, Ordering::Relaxed);
        // Release: the take that reads the count sees the flag.
        WENT.fetch_add(1, Ordering::Release);
    }

    fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }

    /// The shard could not register a handle's descriptor.
    pub(crate) fn refused_registration(&self) {
        self.errors
            .refused_registrations
            .fetch_add(1, Ordering::Relaxed);
    }

    /// The counts as they are now, with `tasks` the shard's live tasks.
    pub(crate) fn read(&self, tasks: usize) -> ShardStats {
        ShardStats {
            worker: self.owner.map(|(_, place)| place),
            runtime: self.runtime().map(crate::runtime::RuntimeId::from_raw),
            backend: self.backend,
            tasks,
            polls: self.polls.load(Ordering::Relaxed),
            parks: self.parks.load(Ordering::Relaxed),
            parked: std::time::Duration::from_nanos(self.parked_nanos.load(Ordering::Relaxed)),
            busy: self
                .born
                .elapsed()
                .saturating_sub(std::time::Duration::from_nanos(
                    self.parked_nanos.load(Ordering::Relaxed),
                )),
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
            remote_wakes: self.remote_wakes.load(Ordering::Relaxed),
            ops_in_flight: self.ops_in_flight.load(Ordering::Relaxed),
            cq_overflows: self.cq_overflows.load(Ordering::Relaxed),
            buffer_bytes: self.buffer_bytes.load(Ordering::Relaxed),
            buffer_cap: self.buffer_cap,
            buffer_exhausted: self.buffer_exhausted.load(Ordering::Relaxed),
            timers: self.timers.load(Ordering::Relaxed),
            handoffs: self.handoffs.load(Ordering::Relaxed),
            longest_poll: std::time::Duration::from_nanos(
                self.longest_poll_nanos.load(Ordering::Relaxed),
            ),
            budget_yields: self.budget_yields.load(Ordering::Relaxed),
            poll_times: self.poll_times.get().map(|buckets| buckets.read()),
            #[cfg(feature = "io-stats")]
            io_latencies: self
                .io_times
                .get()
                .filter(|_| matches!(self.backend, IoBackend::IoUring | IoBackend::Iocp))
                .map(|times| times.read()),
            #[cfg(feature = "io-stats")]
            io: self.io.get().map(|totals| totals.read()),
            errors: self.errors.read(),
        }
    }
}

/// A shard's IO operations, as its own thread adds them: a load and a store.
#[cfg(feature = "io-stats")]
#[derive(Default)]
struct IoTotals {
    reads: AtomicU64,
    writes: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    accepts: AtomicU64,
}

#[cfg(feature = "io-stats")]
impl IoTotals {
    fn add(&self, io: &tally::Taken) {
        for (total, more) in [
            (&self.reads, io.reads),
            (&self.writes, io.writes),
            (&self.bytes_read, io.bytes_read),
            (&self.bytes_written, io.bytes_written),
            (&self.accepts, io.accepts),
        ] {
            total.store(total.load(Ordering::Relaxed) + more, Ordering::Relaxed);
        }
    }

    fn read(&self) -> crate::stats::IoCounts {
        crate::stats::IoCounts {
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            accepts: self.accepts.load(Ordering::Relaxed),
        }
    }
}

/// A log2 histogram of durations as its writers count it: bucket `i` holds
/// the samples of `2^i` nanoseconds up to `2^(i+1)` (bucket 0 the ones under
/// 2). Any thread may add, with one `Relaxed` add.
pub(crate) struct Buckets([AtomicU64; crate::stats::BUCKETS]);

impl Buckets {
    pub(crate) fn new() -> Self {
        Self([const { AtomicU64::new(0) }; crate::stats::BUCKETS])
    }

    pub(crate) fn add(&self, length: std::time::Duration) {
        let nanos = u64::try_from(length.as_nanos()).unwrap_or(u64::MAX);
        self.0[crate::stats::bucket_of(nanos)].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn read(&self) -> crate::stats::Histogram {
        crate::stats::Histogram {
            buckets: core::array::from_fn(|i| self.0[i].load(Ordering::Relaxed)),
        }
    }
}

/// The latencies of a shard's IO operations by kind, from their submission
/// to their completion.
#[cfg(feature = "io-stats")]
pub(crate) struct IoTimes {
    reads: Buckets,
    writes: Buckets,
    accepts: Buckets,
    connects: Buckets,
    files: Buckets,
}

/// The kind of operation a latency is of.
#[cfg(all(completion, feature = "io-stats"))]
#[derive(Clone, Copy)]
pub(crate) enum IoKind {
    Read,
    Write,
    Accept,
    Connect,
    #[cfg(ring)]
    File,
}

#[cfg(feature = "io-stats")]
impl IoTimes {
    fn new() -> Self {
        Self {
            reads: Buckets::new(),
            writes: Buckets::new(),
            accepts: Buckets::new(),
            connects: Buckets::new(),
            files: Buckets::new(),
        }
    }

    /// An operation of `kind` took `length` from submission to completion.
    #[cfg(completion)]
    pub(crate) fn add(&self, kind: IoKind, length: std::time::Duration) {
        match kind {
            IoKind::Read => &self.reads,
            IoKind::Write => &self.writes,
            IoKind::Accept => &self.accepts,
            IoKind::Connect => &self.connects,
            #[cfg(ring)]
            IoKind::File => &self.files,
        }
        .add(length);
    }

    fn read(&self) -> crate::stats::IoLatencies {
        crate::stats::IoLatencies {
            reads: self.reads.read(),
            writes: self.writes.read(),
            accepts: self.accepts.read(),
            connects: self.connects.read(),
            files: self.files.read(),
        }
    }
}

/// Errors counted by kind, and the two counted apart.
struct Errors {
    by_kind: [AtomicU64; KINDS.len()],
    refused_registrations: AtomicU64,
    accepts_out_of_descriptors: AtomicU64,
    cut_datagrams_dropped: AtomicU64,
}

impl Errors {
    const fn new() -> Self {
        Self {
            by_kind: [const { AtomicU64::new(0) }; KINDS.len()],
            refused_registrations: AtomicU64::new(0),
            accepts_out_of_descriptors: AtomicU64::new(0),
            cut_datagrams_dropped: AtomicU64::new(0),
        }
    }

    fn read(&self) -> ErrorCounts {
        ErrorCounts {
            by_kind: core::array::from_fn(|i| self.by_kind[i].load(Ordering::Relaxed)),
            refused_registrations: self.refused_registrations.load(Ordering::Relaxed),
            accepts_out_of_descriptors: self.accepts_out_of_descriptors.load(Ordering::Relaxed),
            cut_datagrams_dropped: self.cut_datagrams_dropped.load(Ordering::Relaxed),
        }
    }
}

/// The errors met on threads that run no shard.
static OFF_SHARD: Errors = Errors::new();

/// Shards that started without their receive group for want of memory.
static GROUPS_MISSING: AtomicU64 = AtomicU64::new(0);

/// A shard started without its receive group for want of memory.
#[cfg(ring)]
pub(crate) fn receive_buffers_missing() {
    GROUPS_MISSING.fetch_add(1, Ordering::Relaxed);
}

/// Runs `f` with the error table of the shard this thread runs, or the one
/// of the threads that run none.
fn errors_here(f: impl FnOnce(&Errors)) {
    super::context::current(|shard| match shard {
        Some(shard) => f(&shard.shared.counters.errors),
        None => f(&OFF_SHARD),
    });
}

/// An IO operation ended in `err`. A "try again" is no error: nuclei
/// waits, or makes the call another way (a unix connect that finds the
/// backlog full).
pub(crate) fn io_error(err: &io::Error) {
    if err.kind() == io::ErrorKind::WouldBlock {
        return;
    }
    let at = kind_index(err.kind());
    errors_here(|errors| {
        errors.by_kind[at].fetch_add(1, Ordering::Relaxed);
    });
}

/// A datagram an owned receive whose task went away had cut to its buffer was dropped.
pub(crate) fn cut_datagram_dropped() {
    errors_here(|errors| {
        errors.cut_datagrams_dropped.fetch_add(1, Ordering::Relaxed);
    });
}

/// An accept ended in `err`, counted already by [`io_error`]: one that
/// failed for want of descriptors is counted as such too.
pub(crate) fn accept_error(err: &io::Error) {
    if out_of_descriptors(err) {
        errors_here(|errors| {
            errors
                .accepts_out_of_descriptors
                .fetch_add(1, Ordering::Relaxed);
        });
    }
}

/// Whether `err` says the process or the system has no descriptor left. Windows answers a socket
/// it cannot make for that, the one an accept takes a connection into among them, with
/// `WSAEMFILE` or `WSAENOBUFS` (`WSASocketW`), and an `AcceptEx` that ends for want of system
/// resources with the status `status_error` names `ERROR_NO_SYSTEM_RESOURCES`.
pub(crate) fn out_of_descriptors(err: &io::Error) -> bool {
    #[cfg(unix)]
    let codes = [libc::EMFILE, libc::ENFILE];
    #[cfg(windows)]
    let codes = {
        use crate::syscore::windows::{ERROR_NO_SYSTEM_RESOURCES, WSAEMFILE, WSAENOBUFS};
        [WSAEMFILE, WSAENOBUFS, ERROR_NO_SYSTEM_RESOURCES]
    };
    err.raw_os_error().is_some_and(|code| codes.contains(&code))
}

/// Whether `err`, an accept's, is the error of the connection it took and not the listener's: its
/// peer reset or aborted it while it waited in the backlog. The listener is as it was, so every
/// accept skips the connection and takes the next one (proofs/tla/accept). On unix that is
/// `ECONNABORTED` (macOS and the BSDs, and Linux when the connection's peer address is gone);
/// `AcceptEx` ends with the reset's status, or with the Win32 error that status maps to.
pub(crate) fn lost_before_accept(err: &io::Error) -> bool {
    #[cfg(unix)]
    let lost = err.kind() == io::ErrorKind::ConnectionAborted;
    #[cfg(windows)]
    let lost = {
        use crate::syscore::windows::{ERROR_NETNAME_DELETED, WSAECONNABORTED, WSAECONNRESET};
        err.raw_os_error().is_some_and(|code| {
            matches!(
                code,
                WSAECONNRESET | WSAECONNABORTED | ERROR_NETNAME_DELETED
            )
        })
    };
    lost
}

/// Every shard made and not taken out yet, by the address of its `Shared`
/// (which the entry's allocation keeps from reuse). Weak: a shard's
/// `Shared` (its descriptors) goes with the shard and its last holder, not
/// with the entry.
fn registry() -> &'static kovan_map::HashMap<usize, Weak<Shared>> {
    static SHARDS: OnceLock<kovan_map::HashMap<usize, Weak<Shared>>> = OnceLock::new();
    SHARDS.get_or_init(|| kovan_map::HashMap::with_capacity(64))
}

/// Shards that went since the last [`take_out_gone`] looked: with none, nothing is there to take
/// out. Every entry that can go went first (a shard's drop says so before its `Shared` can go).
static WENT: AtomicUsize = AtomicUsize::new(0);

/// A shard was made.
pub(crate) fn register(shared: &Arc<Shared>) {
    take_out_gone();
    registry().insert(Arc::as_ptr(shared) as usize, Arc::downgrade(shared));
}

/// Takes out the entries of shards that went. The work scales with the
/// shards alive and gone since the last time.
fn take_out_gone() {
    // Acquire: pairs with `went`'s Release, so every flag counted is seen below. A shard that
    // goes after the swap counts again, for the next take.
    if WENT.load(Ordering::Relaxed) == 0 || WENT.swap(0, Ordering::Acquire) == 0 {
        return;
    }
    let gone: Vec<usize> = registry()
        .iter()
        .filter(|(_, shard)| shard.upgrade().is_none_or(|s| s.counters.is_gone()))
        .map(|(key, _)| key)
        .collect();
    for key in gone {
        drop(registry().remove(&key));
    }
}

/// The counts of every live shard and of the blocking pools.
pub(crate) fn snapshot() -> Stats {
    take_out_gone();
    let mut shards: Vec<ShardStats> = registry()
        .values()
        .filter_map(|shared| shared.upgrade())
        .filter(|shared| !shared.counters.is_gone())
        .map(|shared| shared.stats())
        .collect();
    // Workers first, by runtime and place; then the threads' own shards.
    shards.sort_by_key(|shard| (shard.worker.is_none(), shard.runtime, shard.worker));
    Stats {
        shards,
        blocking_threads: super::blocking::threads_alive(),
        blocking_queue_depth: super::blocking::queued_everywhere(),
        off_shard_errors: OFF_SHARD.read(),
        receive_buffers_missing: GROUPS_MISSING.load(Ordering::Relaxed),
    }
}

/// The counts of the worker shards of runtime `runtime`, by place, with the
/// blocking pool's `threads` and `queued` jobs, and the process's counts of
/// what belongs to no runtime.
pub(crate) fn runtime_snapshot(
    workers: impl Iterator<Item = ShardStats>,
    threads: usize,
    queued: usize,
) -> Stats {
    Stats {
        shards: workers.collect(),
        blocking_threads: threads,
        blocking_queue_depth: queued,
        off_shard_errors: OFF_SHARD.read(),
        receive_buffers_missing: GROUPS_MISSING.load(Ordering::Relaxed),
    }
}

/// A poll on the shard this thread runs spent its budget and yielded. Out
/// of line, so the budget's check, inlined into every IO future, stays as
/// small as it was.
#[cold]
#[inline(never)]
pub(crate) fn budget_yield() {
    super::context::current(|shard| {
        if let Some(shard) = shard {
            shard.shared.counters.budget_yield();
        }
    });
}

#[cfg(all(test, not(loom)))]
mod tests;
