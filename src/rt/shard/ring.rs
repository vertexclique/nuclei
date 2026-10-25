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
//!
//! A multishot receive stays in flight across its arrivals: each completion
//! that says more come (`IORING_CQE_F_MORE`) hands its buffer of the
//! receive group (see `recv`) to the source as a chunk, and only the last
//! ends the operation. Its arrivals are bounded by the group's buffers, so
//! the operations let into the kernel leave the completion queue room for
//! them.

use core::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

use super::recv::RecvGroup;
use crate::io::source::ops::Done;
use crate::io::source::ring::{Arrival, RingMem, RingOp};
use crate::io::sys;
use crate::rt::park::Notifier;
use crate::rt::park::ring::RingPoller;
use crate::syscore::linux::iouring::files::Files;
use crate::syscore::linux::iouring::ops::Ops;
use crate::syscore::linux::iouring::ring::sys::{
    IORING_CQE_F_MORE, IOSQE_FIXED_FILE, cqe_buffer_id,
};
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
    /// The buffers multishot receives draw from; `None` when the
    /// configuration asks for none or the kernel could not make them.
    recv: RefCell<Option<RecvGroup>>,
    /// Multishot receives and accepts submitted, the receives' arrivals,
    /// and the receives that ended as the group ran dry, for a test.
    #[cfg(test)]
    counts: Cell<(usize, usize, usize)>,
}

impl ShardRing {
    /// A ring as `cfg` describes with a table of `files` fixed files,
    /// disabled until its first wait or submission.
    pub(crate) fn new(cfg: &RingConfig, files: u32) -> io::Result<Self> {
        let poller = RingPoller::new(cfg, files)?;
        let recv = match cfg.recv_buffers {
            0 => None,
            entries => match RecvGroup::new(poller.ring(), entries, cfg.recv_buffer_size) {
                Ok(group) => Some(group),
                // A count or a size no group can have is the configuration's
                // mistake, said at once.
                Err(err) if err.kind() == io::ErrorKind::InvalidInput => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("io_uring recv_buffers and recv_buffer_size: {err}"),
                    ));
                }
                // Short of memory: each read takes a buffer of the pool.
                Err(_) => None,
            },
        };
        // Room in the completion queue for every arrival the group's buffers
        // can hold.
        let arrivals = recv
            .as_ref()
            .map_or(0, |g: &RecvGroup| usize::from(g.entries()));
        let cap = (poller.cq_entries() as usize)
            .saturating_sub(arrivals)
            .max(1);
        Ok(Self {
            poller: RefCell::new(poller),
            ops: RefCell::new(Ops::new(u32::MAX)),
            files: RefCell::new(Files::new(files)),
            room: RefCell::new(VecDeque::new()),
            in_kernel: Cell::new(0),
            cap,
            cqes: RefCell::new(Vec::with_capacity(256)),
            recv: RefCell::new(recv),
            #[cfg(test)]
            counts: Cell::new((0, 0, 0)),
        })
    }

    /// The group id multishot receives draw from, if the ring has a group.
    pub(crate) fn recv_group(&self) -> Option<u16> {
        self.recv.borrow().as_ref().map(RecvGroup::bgid)
    }

    /// The size of the group's buffers, if the ring has a group.
    pub(crate) fn recv_buf_size(&self) -> Option<u32> {
        self.recv.borrow().as_ref().map(RecvGroup::buf_size)
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
        #[cfg(test)]
        if matches!(
            op.mem,
            RingMem::RecvMulti { .. } | RingMem::RecvFromMulti { .. } | RingMem::AcceptMulti { .. }
        ) {
            let (armed, arrived, dry) = self.counts.get();
            self.counts.set((armed + 1, arrived, dry));
        }
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
        self.take_back();
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
        if let Some(bid) = cqe_buffer_id(cqe.flags) {
            self.arrived(cqe.user_data, bid, cqe.res);
        } else if cqe.res >= 0 {
            self.accepted(cqe.user_data, cqe.res);
        }
        // More of a multishot receive's arrivals come: it stays in flight.
        if cqe.flags & IORING_CQE_F_MORE != 0 {
            return;
        }
        let Some(op) = self.ops.borrow_mut().remove(cqe.user_data) else {
            return;
        };
        self.in_kernel.set(self.in_kernel.get().saturating_sub(1));
        self.admit_waiting();
        self.end(op, cqe.res);
    }

    /// Buffer `bid` of the receive group came with `res` bytes for the
    /// multishot receive of `user_data`: its source gets them as a chunk.
    /// Past the cap of bytes it holds, the receive is stopped; its source
    /// asks for another once its reader took them.
    fn arrived(&self, user_data: u64, bid: u16, res: i32) {
        let mut stop = false;
        let delivery = {
            let mut recv = self.recv.borrow_mut();
            let Some(group) = recv.as_mut() else {
                return;
            };
            let mut ops = self.ops.borrow_mut();
            let held = ops.get_mut(user_data).and_then(|op| match &mut op.mem {
                RingMem::RecvMulti { held, stopping } => Some((op.epoch, held, stopping, false)),
                RingMem::RecvFromMulti { held, stopping, .. } => {
                    Some((op.epoch, held, stopping, true))
                }
                _ => None,
            });
            match (u32::try_from(res), held) {
                (Ok(len @ 1..), Some((epoch, held, stopping, datagram))) => {
                    match group.hand_out(user_data, bid, len) {
                        Some(chunk) => {
                            *held += len as usize;
                            if *held > group.cap() && !*stopping {
                                *stopping = true;
                                stop = true;
                            }
                            ops.get_mut(user_data).map(|op| {
                                (std::sync::Arc::clone(&op.source), chunk, epoch, datagram)
                            })
                        }
                        None => None,
                    }
                }
                // No bytes, or no receive to give them to: the buffer goes
                // back at once.
                _ => {
                    group.give_back(bid);
                    None
                }
            }
        };
        if stop {
            self.cancel(user_data);
        }
        // Handed over with nothing borrowed: the source may submit at once.
        if let Some((source, chunk, epoch, datagram)) = delivery {
            #[cfg(test)]
            {
                let (armed, arrived, dry) = self.counts.get();
                self.counts.set((armed, arrived + 1, dry));
            }
            let arrival = if datagram {
                // A buffer not laid out as the kernel says is only given back.
                match crate::io::source::ring::datagram_of(chunk, source.kind()) {
                    Some((payload, from)) => Arrival::Datagram(payload, from),
                    None => return,
                }
            } else {
                Arrival::Chunk(chunk)
            };
            source.ring_arrival(arrival, epoch);
        }
    }

    /// A completion `res` of the operation of `user_data` that carries no
    /// buffer: for a multishot accept, a connection (its descriptor), handed
    /// to the source as it comes. Past the cap of connections one accept
    /// hands over, the accept is stopped; its source asks for another once
    /// its tasks took them.
    fn accepted(&self, user_data: u64, res: i32) {
        let delivery = {
            let mut ops = self.ops.borrow_mut();
            let Some(op) = ops.get_mut(user_data) else {
                return;
            };
            let RingMem::AcceptMulti { handed, stopping } = &mut op.mem else {
                return;
            };
            *handed += 1;
            let stop = *handed > ACCEPT_CAP && !*stopping;
            if stop {
                *stopping = true;
            }
            // SAFETY: the kernel made this descriptor for us.
            let conn =
                unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(res) };
            (std::sync::Arc::clone(&op.source), conn, op.epoch, stop)
        };
        let (source, conn, epoch, stop) = delivery;
        if stop {
            self.cancel(user_data);
        }
        source.ring_arrival(Arrival::Conn(conn), epoch);
    }

    /// Takes in the buffers chunks gave back and gives them to the kernel
    /// again, counting each off the receive it went to.
    fn take_back(&self) {
        let mut recv = self.recv.borrow_mut();
        let Some(group) = recv.as_mut() else {
            return;
        };
        let mut ops = self.ops.borrow_mut();
        group.take_back(|user_data, len| {
            if let Some(RingMem::RecvMulti { held, .. } | RingMem::RecvFromMulti { held, .. }) =
                ops.get_mut(user_data).map(|op| &mut op.mem)
            {
                *held = held.saturating_sub(len as usize);
            }
        });
    }

    /// `op` ended with `res`: its slot is named once less, and its source
    /// gets the completion.
    fn end(&self, op: RingOp, res: i32) {
        #[cfg(test)]
        if res == -libc::ENOBUFS && matches!(op.mem, RingMem::RecvMulti { .. }) {
            let (armed, arrived, dry) = self.counts.get();
            self.counts.set((armed, arrived, dry + 1));
        }
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
        if !stuck.is_empty() {
            // A receive still in the kernel may write the group's buffers:
            // they stay registered and mapped for good.
            core::mem::forget(self.recv.borrow_mut().take());
        }
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

/// The connections one multishot accept hands over before the ring stops
/// it: its source asks for another once its tasks took them.
const ACCEPT_CAP: usize = 256;

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

    /// Multishot receives submitted so far, the arrivals they handed out,
    /// and those that ended as the group ran dry.
    pub(crate) fn multishot_counts(&self) -> (usize, usize, usize) {
        self.counts.get()
    }

    /// Bytes of the group's buffers handed out and not given back, whether
    /// or not the receive they went to is still in flight.
    pub(crate) fn held(&self) -> usize {
        self.take_back();
        self.recv.borrow().as_ref().map_or(0, RecvGroup::out)
    }

    /// Multishot receives and accepts in flight.
    pub(crate) fn receives_in_flight(&self) -> usize {
        self.ops
            .borrow()
            .iter()
            .filter(|op| {
                matches!(
                    op.mem,
                    RingMem::RecvMulti { .. }
                        | RingMem::RecvFromMulti { .. }
                        | RingMem::AcceptMulti { .. }
                )
            })
            .count()
    }

    /// The bytes one receive may hold before the ring stops it.
    pub(crate) fn recv_cap(&self) -> Option<usize> {
        self.recv.borrow().as_ref().map(RecvGroup::cap)
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
#[cfg(test)]
mod tests_recv;
