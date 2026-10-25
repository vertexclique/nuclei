//! A shard's ring, as its own thread uses it: the poller, the operations in
//! flight, the source files, and the operations waiting for room.
//!
//! Every entry for a source names the source's file by its slot (see
//! `syscore::linux::iouring::files`): a fixed file of the ring's table
//! (`IOSQE_FIXED_FILE`), or past the table the ring's own copy of the
//! descriptor. Either way the ring holds its own reference to the file, so
//! a handle that closes its descriptor on any thread never changes what an
//! entry reaches, queued or submitted. A slot, and the file it holds, is let
//! go only once no entry names it.
//!
//! An operation gets its user data as it is submitted, and keeps it until
//! its completion: one can be cancelled by it at any time. At most as many
//! are in the kernel as the completion queue holds; the rest wait here, in
//! order, entry and memory together, and go in as completions free room.
//! One that is cancelled or whose source goes while it waits ends here,
//! never submitted. A side has one operation out at a time, so the
//! operations here are bounded by the sources.

use core::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

use crate::io::source::ops::Done;
use crate::io::source::ring::RingOp;
use crate::io::sys;
use crate::rt::park::Notifier;
use crate::rt::park::ring::RingPoller;
use crate::syscore::linux::iouring::files::Files;
use crate::syscore::linux::iouring::ops::Ops;
use crate::syscore::linux::iouring::ring::sys::IOSQE_FIXED_FILE;
use crate::syscore::linux::iouring::ring::{IoUringCqe, RingConfig, Sqe};

/// A shard's ring.
pub(crate) struct ShardRing {
    poller: RefCell<RingPoller>,
    ops: RefCell<Ops<RingOp>>,
    files: RefCell<Files>,
    /// Operations submitted but not in the kernel yet, by user data, with
    /// their entries, in order.
    room: RefCell<VecDeque<(u64, Sqe)>>,
    /// Operations in the kernel, and the most it may hold.
    in_kernel: Cell<usize>,
    cap: usize,
    /// Completions of the last wait, reused.
    cqes: RefCell<Vec<IoUringCqe>>,
}

impl ShardRing {
    /// A ring as `cfg` describes with a table of `files` fixed files,
    /// disabled until its first wait or submission.
    pub(crate) fn new(cfg: &RingConfig, files: u32) -> io::Result<Self> {
        let poller = RingPoller::new(cfg, files)?;
        let cap = poller.cq_entries() as usize;
        Ok(Self {
            poller: RefCell::new(poller),
            ops: RefCell::new(Ops::new(u32::MAX)),
            files: RefCell::new(Files::new(files)),
            room: RefCell::new(VecDeque::new()),
            in_kernel: Cell::new(0),
            cap,
            cqes: RefCell::new(Vec::with_capacity(256)),
        })
    }

    pub(crate) fn notifier(&self) -> Notifier {
        self.poller.borrow().notifier()
    }

    /// Submits `op` with its entry `sqe` (whose pointers point into `op`'s
    /// memory, which lives on the heap and does not move with it), on the
    /// slot `op.file`, and returns its user data. With the kernel full it
    /// waits for room; its slot is named from now on either way.
    pub(crate) fn submit(&self, op: RingOp, sqe: Sqe) -> u64 {
        self.files.borrow_mut().named(op.file);
        let sqe = match self.files.borrow().copy_of(op.file) {
            Some(copy) => sqe.fd(copy),
            None => sqe.flags(IOSQE_FIXED_FILE),
        };
        op.source.op_out();
        let inserted = self.ops.borrow_mut().insert(op);
        let user_data = match inserted {
            Ok(user_data) => user_data,
            // Past 2^32 - 2 operations at once (two per source at most).
            Err(op) => {
                self.end(op, -libc::ENOBUFS);
                return IGNORED;
            }
        };
        let sqe = sqe.user_data(user_data);
        if self.room.borrow().is_empty() && self.in_kernel.get() < self.cap {
            self.in_kernel.set(self.in_kernel.get() + 1);
            self.poller.borrow_mut().push(sqe);
        } else {
            self.room.borrow_mut().push_back((user_data, sqe));
        }
        user_data
    }

    /// Moves waiting operations into the kernel while it has room.
    fn admit_waiting(&self) {
        while self.in_kernel.get() < self.cap {
            let Some((_, sqe)) = self.room.borrow_mut().pop_front() else {
                return;
            };
            self.in_kernel.set(self.in_kernel.get() + 1);
            self.poller.borrow_mut().push(sqe);
        }
    }

    /// Cancels the operation of `user_data`: at once if it waits for room,
    /// else by the kernel (its completion says what it did).
    pub(crate) fn cancel(&self, user_data: u64) {
        let waiting = {
            let mut room = self.room.borrow_mut();
            room.iter()
                .position(|(u, _)| *u == user_data)
                .and_then(|at| room.remove(at))
        };
        if waiting.is_some() {
            if let Some(op) = self.ops.borrow_mut().remove(user_data) {
                self.end(op, -libc::ECANCELED);
            }
            return;
        }
        if self.ops.borrow_mut().get_mut(user_data).is_some() {
            self.poller
                .borrow_mut()
                .push(Sqe::async_cancel(user_data).user_data(IGNORED));
        }
    }

    /// A slot holding the file of `fd`, which the caller keeps open for
    /// the call: one of the ring's table, or past a full table, the ring's
    /// own copy of `fd`.
    pub(crate) fn attach(&self, fd: RawFd) -> io::Result<u32> {
        let fixed = self.files.borrow_mut().take();
        match fixed {
            Some(slot) => match self.poller.borrow().update_file(slot, fd) {
                Ok(()) => Ok(slot),
                Err(err) => {
                    self.files.borrow_mut().release(slot);
                    Err(err)
                }
            },
            None => {
                let copy = sys::duplicate(fd)?;
                Ok(self.files.borrow_mut().take_copy(copy))
            }
        }
    }

    /// The source of `slot` went: its sides' operations `out` are
    /// cancelled, and the slot, with its file, is let go once no entry
    /// names it (at once if none is out). A cancel by user data needs no
    /// file, so nothing else keeps the slot.
    pub(crate) fn detach(&self, slot: u32, out: [Option<u64>; 2]) {
        for user_data in out.into_iter().flatten() {
            self.cancel(user_data);
        }
        let freed = self.files.borrow_mut().release(slot);
        self.freed(slot, freed);
    }

    /// A slot that came free: its fixed file is let go (a copy closed as
    /// it was freed).
    fn freed(&self, slot: u32, freed: bool) {
        if freed && self.files.borrow().is_fixed(slot) {
            // A failure leaves the file in the free slot until the slot's
            // next source replaces it.
            let _ = self.poller.borrow().update_file(slot, -1);
        }
    }

    /// Submits what is queued and waits up to `timeout` for completions,
    /// which are handed to their sources. Returns how many it handled and
    /// whether a wake came.
    pub(crate) fn wait(&self, timeout: Option<Duration>) -> io::Result<(usize, bool)> {
        let mut cqes = core::mem::take(&mut *self.cqes.borrow_mut());
        let woken = self.poller.borrow_mut().wait(timeout, &mut cqes);
        let handled = cqes.len();
        for cqe in cqes.drain(..) {
            self.complete(cqe);
        }
        *self.cqes.borrow_mut() = cqes;
        Ok((handled, woken?))
    }

    fn complete(&self, cqe: IoUringCqe) {
        let Some(op) = self.ops.borrow_mut().remove(cqe.user_data) else {
            return;
        };
        self.in_kernel.set(self.in_kernel.get().saturating_sub(1));
        self.admit_waiting();
        self.end(op, cqe.res);
    }

    /// `op` ended with `res`: its slot is named once less, and its source
    /// gets the completion.
    fn end(&self, op: RingOp, res: i32) {
        let freed = self.files.borrow_mut().done(op.file);
        self.freed(op.file, freed);
        let kind = op.source.kind();
        op.source.op_done(Done {
            outcome: op.mem.complete(res, kind),
            epoch: op.epoch,
            read: op.read,
        });
    }

    /// The shard is going, its sources detached: every operation still out
    /// is cancelled and its completion waited for, for at most
    /// [`SHUTDOWN_WAIT`]. One that never completes keeps its memory for
    /// good (the kernel may still write it), and is counted (`leaked`).
    pub(crate) fn shut_down(&self) {
        let waiting: Vec<RingOp> = {
            let mut room = self.room.borrow_mut();
            let mut ops = self.ops.borrow_mut();
            room.drain(..).filter_map(|(u, _)| ops.remove(u)).collect()
        };
        for op in waiting {
            // Never submitted: nothing points at its memory.
            self.end(op, -libc::ECANCELED);
        }
        if self.ops.borrow().len() > 0 {
            self.poller
                .borrow_mut()
                .push(Sqe::async_cancel_any(true).user_data(IGNORED));
        }
        self.settle(|| {});
        let stuck: Vec<RingOp> = self.ops.borrow_mut().drain().collect();
        LEAKED.fetch_add(stuck.len(), core::sync::atomic::Ordering::Relaxed);
        core::mem::forget(stuck);
    }
}

impl ShardRing {
    /// Submits what is queued and waits until no operation is out, for at
    /// most [`SHUTDOWN_WAIT`]: what its sources cancelled as they went.
    /// `between` runs before each wait (the shard's mailbox, where a
    /// source's holder on another thread leaves its cancel).
    pub(crate) fn settle(&self, mut between: impl FnMut()) {
        let deadline = std::time::Instant::now() + SHUTDOWN_WAIT;
        loop {
            between();
            if self.ops.borrow().len() == 0 {
                return;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero()
                || self
                    .wait(Some(left.min(Duration::from_millis(10))))
                    .is_err()
            {
                return;
            }
        }
    }
}

/// The user data of the ring's own cancels, whose completions say nothing
/// to anyone: no operation of the slab has it (its index is the one the
/// slab never hands out), and it is not the wake's.
const IGNORED: u64 = u32::MAX as u64;

/// How long a shard that goes waits for its ring's cancelled operations.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

static LEAKED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Operations whose completion never came as their shard went: their memory
/// is kept for good.
#[cfg(test)]
pub(crate) fn leaked() -> usize {
    LEAKED.load(core::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
impl ShardRing {
    /// Slots held or still named, and of them the ones past the table.
    pub(crate) fn files_in_use(&self) -> (usize, usize) {
        let files = self.files.borrow();
        (files.in_use(), files.copies_in_use())
    }
}

#[cfg(test)]
thread_local! {
    /// The table a ring made on this thread gets, for a test.
    pub(crate) static TABLE_FOR_TEST: Cell<Option<u32>> = const { Cell::new(None) };
}

/// The fixed-file table of a ring: the process's descriptor limit (no more
/// sources can be open while it holds), at most [`MAX_TABLE`], whose slots
/// cost kernel memory per ring. Sources past it run on copies.
pub(crate) fn table_len() -> u32 {
    #[cfg(test)]
    if let Some(len) = TABLE_FOR_TEST.with(Cell::get) {
        return len;
    }
    // SAFETY: plain data; the call fills it.
    let mut limit: libc::rlimit = unsafe { core::mem::zeroed() };
    // SAFETY: a plain syscall writing `limit`.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        1024
    };
    u32::try_from(soft)
        .unwrap_or(MAX_TABLE)
        .clamp(64, MAX_TABLE)
}

/// The most fixed files a ring's table holds.
const MAX_TABLE: u32 = 1 << 14;

#[cfg(test)]
mod tests;
