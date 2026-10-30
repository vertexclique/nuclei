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
    /// The shard's place among its runtime's workers.
    worker: Option<usize>,
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
    /// bytes, worker `worker` of its runtime if it is one.
    pub(crate) fn new(backend: IoBackend, buffer_cap: usize, worker: Option<usize>) -> Self {
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
            worker,
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

    /// The shard went: `stats` lists it no more.
    pub(crate) fn went(&self) {
        self.gone.store(true, Ordering::Relaxed);
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
            worker: self.worker,
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
            errors: self.errors.read(),
        }
    }
}

/// Errors counted by kind, and the two counted apart.
struct Errors {
    by_kind: [AtomicU64; KINDS.len()],
    refused_registrations: AtomicU64,
    accepts_out_of_descriptors: AtomicU64,
}

impl Errors {
    const fn new() -> Self {
        Self {
            by_kind: [const { AtomicU64::new(0) }; KINDS.len()],
            refused_registrations: AtomicU64::new(0),
            accepts_out_of_descriptors: AtomicU64::new(0),
        }
    }

    fn read(&self) -> ErrorCounts {
        ErrorCounts {
            by_kind: core::array::from_fn(|i| self.by_kind[i].load(Ordering::Relaxed)),
            refused_registrations: self.refused_registrations.load(Ordering::Relaxed),
            accepts_out_of_descriptors: self.accepts_out_of_descriptors.load(Ordering::Relaxed),
        }
    }
}

/// The errors met on threads that run no shard.
static OFF_SHARD: Errors = Errors::new();

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

/// Whether `err` says the process or the system has no descriptor left.
pub(crate) fn out_of_descriptors(err: &io::Error) -> bool {
    #[cfg(unix)]
    let codes = [libc::EMFILE, libc::ENFILE];
    #[cfg(windows)]
    let codes = [crate::syscore::windows::WSAEMFILE];
    err.raw_os_error().is_some_and(|code| codes.contains(&code))
}

/// Every shard made and not taken out yet, by the address of its `Shared`
/// (which the entry's allocation keeps from reuse). Weak: a shard's
/// `Shared` (its descriptors) goes with the shard and its last holder, not
/// with the entry.
fn registry() -> &'static kovan_map::HashMap<usize, Weak<Shared>> {
    static SHARDS: OnceLock<kovan_map::HashMap<usize, Weak<Shared>>> = OnceLock::new();
    SHARDS.get_or_init(|| kovan_map::HashMap::with_capacity(64))
}

/// A shard was made.
pub(crate) fn register(shared: &Arc<Shared>) {
    take_out_gone();
    registry().insert(Arc::as_ptr(shared) as usize, Arc::downgrade(shared));
}

/// Takes out the entries of shards that went. The work scales with the
/// shards alive and gone since the last time.
fn take_out_gone() {
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
    // Workers first, by place; then the threads' own shards.
    shards.sort_by_key(|shard| (shard.worker.is_none(), shard.worker));
    Stats {
        shards,
        blocking_threads: super::blocking::threads_alive(),
        off_shard_errors: OFF_SHARD.read(),
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
