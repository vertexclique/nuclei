//! The IO operations a thread has completed, counted in plain cells of its
//! own and drained into the counters of the shard it runs as the shard
//! publishes (see [`Counters::publish`](super::Counters::publish)), once a
//! runtime of the process counts them (`Builder::count_io`): until then each
//! count is one load of a flag and nothing else.
//!
//! The cells hold no destructor, so a count is a thread-local load and
//! store: no atomic, no lookup of the shard, no clock. IO a thread
//! completes while it runs no shard (a blocking pool's own calls, a thread
//! outside the runtime) stays in that thread's cells and is counted by no
//! shard.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

/// Whether a runtime of the process counts its IO operations.
static COUNTING: AtomicBool = AtomicBool::new(false);

/// A runtime counts its IO operations: every thread counts from now on.
pub(crate) fn start() {
    COUNTING.store(true, Ordering::Relaxed);
}

/// Whether threads count their IO operations.
#[inline(always)]
pub(crate) fn counting() -> bool {
    COUNTING.load(Ordering::Relaxed)
}

struct Cells {
    reads: Cell<u64>,
    writes: Cell<u64>,
    bytes_read: Cell<u64>,
    bytes_written: Cell<u64>,
    accepts: Cell<u64>,
}

thread_local! {
    static CELLS: Cells = const {
        Cells {
            reads: Cell::new(0),
            writes: Cell::new(0),
            bytes_read: Cell::new(0),
            bytes_written: Cell::new(0),
            accepts: Cell::new(0),
        }
    };
}

/// What a thread's cells held when they were drained.
#[derive(Default)]
pub(crate) struct Taken {
    pub(crate) reads: u64,
    pub(crate) writes: u64,
    pub(crate) bytes_read: u64,
    pub(crate) bytes_written: u64,
    pub(crate) accepts: u64,
}

// Each count below is the flag's load and a branch, inlined where the call is; the counting
// itself is out of line, so a process that counts nothing keeps its IO paths as they were.

/// A read or receive that returned `bytes` (none: the end of the stream).
#[inline(always)]
pub(crate) fn read(bytes: usize) {
    if counting() {
        add_read(bytes);
    }
}

/// A write or send that took `bytes`.
#[inline(always)]
pub(crate) fn wrote(bytes: usize) {
    if counting() {
        add_write(bytes);
    }
}

/// `count` connections accepted.
#[inline(always)]
pub(crate) fn accepted(count: usize) {
    if counting() {
        add_accepts(count);
    }
}

/// A call's result, counted as a read when it brought bytes or the end.
#[inline(always)]
pub(crate) fn reading(result: &io::Result<usize>) {
    if counting() {
        count_reading(result);
    }
}

/// As [`reading`], for a receive that also names the sender.
#[inline(always)]
pub(crate) fn received<A>(result: &io::Result<(usize, A)>) {
    if counting() {
        count_received(result);
    }
}

/// A call's result, counted as a write when it took bytes.
#[inline(always)]
pub(crate) fn writing(result: &io::Result<usize>) {
    if counting() {
        count_writing(result);
    }
}

/// An accept's result, counted when it brought a connection.
#[cfg(readiness)]
#[inline(always)]
pub(crate) fn accepting<A>(result: &io::Result<A>) {
    if result.is_ok() {
        accepted(1);
    }
}

#[inline(never)]
fn add_read(bytes: usize) {
    CELLS.with(|c| {
        c.reads.set(c.reads.get() + 1);
        c.bytes_read.set(c.bytes_read.get() + bytes as u64);
    });
}

#[inline(never)]
fn add_write(bytes: usize) {
    CELLS.with(|c| {
        c.writes.set(c.writes.get() + 1);
        c.bytes_written.set(c.bytes_written.get() + bytes as u64);
    });
}

#[inline(never)]
fn add_accepts(count: usize) {
    CELLS.with(|c| c.accepts.set(c.accepts.get() + count as u64));
}

#[inline(never)]
fn count_reading(result: &io::Result<usize>) {
    if let Ok(n) = result {
        add_read(*n);
    }
}

#[inline(never)]
fn count_received<A>(result: &io::Result<(usize, A)>) {
    if let Ok((n, _)) = result {
        add_read(*n);
    }
}

#[inline(never)]
fn count_writing(result: &io::Result<usize>) {
    if let Ok(n @ 1..) = result {
        add_write(*n);
    }
}

/// Empties this thread's cells.
pub(crate) fn take() -> Taken {
    CELLS.with(|c| Taken {
        reads: c.reads.take(),
        writes: c.writes.take(),
        bytes_read: c.bytes_read.take(),
        bytes_written: c.bytes_written.take(),
        accepts: c.accepts.take(),
    })
}
