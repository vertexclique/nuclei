//! A shard's ring, as its own thread uses it: the poller, the operations in
//! flight, the source files, and the operations waiting for room.
//!
//! Every entry for a source names the source's file by its slot (see
//! `syscore::linux::iouring::files`): a fixed file of the ring's table
//! (`IOSQE_FIXED_FILE`), or past the table the source's own descriptor,
//! which the ring never copies. A source that goes ([`ShardRing::detach`])
//! has its operations cancelled and every entry naming its slot handed to
//! the kernel at once, which holds each request's file from then on, and its
//! file taken out of the table at once: so the kernel closes the file as the
//! handle's drop returns (once its requests let go of it), and the handle's
//! object, which a lease keeps open until then, closes only once no entry
//! that names its number waits to reach the kernel. The slot is handed out
//! again only once no entry names it.
//!
//! An operation gets its user data as it is submitted, and keeps it until
//! its completion: one can be cancelled by it at any time. A side has one
//! operation out at a time, so the operations in flight are bounded by the
//! sources, apart from the zero-copy sends below.
//!
//! A zero-copy send ends for its side at its first completion, and stays here
//! only to hold its write's buffer until the kernel's notification, which
//! comes once the peer acknowledged the bytes: the side goes on meanwhile, so
//! one side may leave several. Each holds at most a chunk the socket's send
//! buffer took, and none names its slot once it only waits. Only a ring a
//! keeper can take sends zero-copy: as its shard goes, what is still out goes
//! to the keeper with the ring (see `keeper`), so a buffer is kept until its
//! notification and no longer.
//!
//! A multishot receive stays in flight across its arrivals: each completion
//! that says more come (`IORING_CQE_F_MORE`) hands its buffer of the
//! receive group (see `recv`) to the source as a chunk, and only the last
//! ends the operation. Its arrivals are bounded by the group's buffers, so
//! the operations let into the kernel leave the completion queue room for
//! them.
//!
//! The group is sized to the ring: at most half its completion queue, so a
//! small ring (made so under memory pressure) is never filled by arrivals.
//! The rest of the queue is the cap on the finite operations in the kernel,
//! the files' reads and writes: past it they wait here, in order, entry and
//! memory together, and go in as completions free room. Every operation of a
//! source waits on its peer for as long as the peer pleases (a receive or an
//! accept until it sends or connects, a send or a poll for room until it
//! reads), and a child's exit watch on the child, so they go into the kernel
//! at once and never count against the cap: idle or slow peers would
//! otherwise hold the room another connection's reply, the next accept or a
//! file call needs (proofs/tla/admission). The multishot receives' arrivals
//! are bounded by the group's buffers, which the queue keeps room for. An
//! accept's connections (up to [`ACCEPT_CAP`] each) and the completion of
//! each of these operations have no room kept: past the queue the kernel
//! holds them (`IORING_FEAT_NODROP`, which the poller requires) until the
//! poller takes them, and none is lost.

use core::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::time::Duration;

use super::Shard;
use super::recv::RecvGroup;
use crate::fs::ring::FileOp;
use crate::io::side::After;
use crate::io::source::ops::Done;
use crate::io::source::requests::Lease;
use crate::io::source::ring::{Arrival, RingMem, RingOp};
use crate::rt::backend::with_room;
use crate::rt::park::Notifier;
use crate::rt::park::ring::{CqeBatch, RingPoller};
use crate::syscore::linux::iouring::files::Files;
use crate::syscore::linux::iouring::ops::Ops;
use crate::syscore::linux::iouring::ring::sys::{
    IORING_CQE_F_BUF_MORE, IORING_CQE_F_BUFFER, IORING_CQE_F_MORE, IORING_CQE_F_NOTIF,
    IOSQE_FIXED_FILE, cqe_buffer_id,
};
use crate::syscore::linux::iouring::ring::{IoUringCqe, RingConfig, Sqe};
use exits::ExitOp;

/// A shard's ring.
pub(crate) struct ShardRing {
    poller: RefCell<RingPoller>,
    ops: RefCell<Ops<RingOp>>,
    /// Files' reads and writes in flight, their user data marked with
    /// [`FILE_TAG`].
    file_ops: RefCell<Ops<FileOp>>,
    /// Children's exits watched, their user data marked with [`FILE_TAG`]
    /// and [`EXIT_TAG`].
    exit_ops: RefCell<Ops<ExitOp>>,
    /// The kernel has `IORING_OP_WAITID` (Linux 6.7).
    waitid: bool,
    /// The kernel has `IORING_OP_SEND_ZC` (Linux 6.0).
    send_zc: bool,
    /// A zero-copy send was refused for lack of lockable memory: the ring sends with copies from
    /// then on.
    zc_denied: Cell<bool>,
    /// The least a zero-copy write must have left for its next send to go zero-copy: the
    /// runtime's `zero_copy_threshold`, set as the worker's shard is made.
    zero_copy_min: Cell<usize>,
    /// Any thread may submit to the ring and its completions are posted without its thread (it
    /// is not single-issuer with deferred task work), so a keeper can take it as its shard goes.
    reapable: bool,
    files: RefCell<Files>,
    /// Slots of sources that went whose entries did not all reach the
    /// kernel yet (it held completions it had no room for), with the leases
    /// that keep the sources' objects open until they do.
    leaving: RefCell<Vec<(u32, Option<Lease>)>>,
    /// Files' calls submitted but not in the kernel yet, by user data, with
    /// their entries, in order.
    room: RefCell<VecDeque<(u64, Sqe)>>,
    /// Files' calls in the kernel, the only operations the cap counts, and
    /// the most it may hold.
    in_kernel: Cell<usize>,
    cap: usize,
    /// Write sides' operations in the kernel: no cap holds them back (a peer
    /// that never reads keeps one for good), but most end within a moment.
    writes: Cell<usize>,
    /// The buffers multishot receives draw from; `None` when the
    /// configuration asks for none, or once the ring is shut down.
    recv: RefCell<Option<RecvGroup>>,
    /// Multishot receives and accepts submitted, the receives' arrivals,
    /// and the receives that ended as the group ran dry, for a test.
    #[cfg(test)]
    counts: Cell<(usize, usize, usize)>,
    /// Files' calls submitted, for a test.
    #[cfg(test)]
    file_calls: Cell<usize>,
    /// Zero-copy sends' first completions and notifications, for a test.
    #[cfg(test)]
    zc_seen: Cell<(usize, usize)>,
}

impl ShardRing {
    /// A ring as `cfg` describes with a table of `files` fixed files,
    /// disabled until its first wait or submission.
    pub(crate) fn new(cfg: &RingConfig, files: u32) -> io::Result<Self> {
        let poller = RingPoller::new(
            &RingConfig {
                taskrun_flag: crate::syscore::linux::iouring::ring::taskrun::flag_works(),
                ..*cfg
            },
            files,
        )?;
        let recv = match group_entries(cfg.recv_buffers, poller.cq_entries()) {
            0 => None,
            entries => match RecvGroup::new(poller.ring(), entries, cfg.recv_buffer_size) {
                Ok(group) => Some(group),
                // A count or a size no group can have is the configuration's
                // mistake, said at once. Only nuclei's own checks answer this
                // way: the kernel's refusal of the feature is `Unsupported`.
                Err(err) if err.kind() == io::ErrorKind::InvalidInput => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("io_uring recv_buffers and recv_buffer_size: {err}"),
                    ));
                }
                // No memory for the group: the shard starts without it, said
                // and counted (`went_without`), apart from the ring's own
                // shortage, which the ring answers by shrinking (`with_room`).
                Err(err) if err.kind() == io::ErrorKind::OutOfMemory => {
                    super::recv::went_without(entries, cfg.recv_buffer_size, &err);
                    None
                }
                // Anything else, a kernel's refusal included, is
                // the ring's failure: a shard never runs without the group
                // its configuration asked for, unseen.
                Err(err) => return Err(err),
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
        use crate::syscore::linux::iouring::ring::sys::Opcode;
        let probed = poller.ring().register_probe().unwrap_or_default();
        let waitid = probed.contains(Opcode::Waitid);
        let send_zc = probed.contains(Opcode::SendZc);
        Ok(Self {
            poller: RefCell::new(poller),
            // Below the file tag's bit, so a source's user data never has it.
            ops: RefCell::new(Ops::new((1 << 31) - 1)),
            file_ops: RefCell::new(Ops::new(1 << 30)),
            exit_ops: RefCell::new(Ops::new(1 << 30)),
            waitid,
            send_zc,
            zc_denied: Cell::new(false),
            zero_copy_min: Cell::new(crate::io::source::ring::SEND_ZC_MIN),
            reapable: cfg.multi_issuer,
            files: RefCell::new(Files::new(files)),
            leaving: RefCell::new(Vec::new()),
            room: RefCell::new(VecDeque::new()),
            in_kernel: Cell::new(0),
            cap,
            writes: Cell::new(0),
            recv: RefCell::new(recv),
            #[cfg(test)]
            counts: Cell::new((0, 0, 0)),
            #[cfg(test)]
            file_calls: Cell::new(0),
            #[cfg(test)]
            zc_seen: Cell::new((0, 0)),
        })
    }

    /// The ring of a shard of `cfg`: with fewer entries while the system has
    /// no memory for `cfg.entries`, down to [`MIN_RING_ENTRIES`], and the
    /// table of [`table_len`] fixed files. The one place a shard's ring is
    /// made, so [`check`] sees exactly what a shard gets.
    pub(crate) fn for_shard(cfg: RingConfig) -> io::Result<Self> {
        with_room(cfg.entries, MIN_RING_ENTRIES, |entries| {
            Self::new(&RingConfig { entries, ..cfg }, table_len())
        })
    }

    /// Makes the calling thread the ring's issuer, as a shard's first wait
    /// or submission does.
    pub(crate) fn enable(&self) -> io::Result<()> {
        self.poller.borrow_mut().enable()
    }

    /// Whether operations whose completions come within a moment are out: files' calls (in the
    /// kernel or waiting for room) and a write side's. A read side's waits on a peer for as long
    /// as the peer is idle, and a child's exit watch on the child, so neither is a sign of one.
    pub(crate) fn expects_completions(&self) -> bool {
        self.in_kernel.get() > 0 || self.writes.get() > 0 || !self.room.borrow().is_empty()
    }

    /// Keeps the buffer `bid` of the group `back` for the next take-in, if the ring's group is
    /// that; `false` when it is not or the group is borrowed (the buffer then goes the long way).
    pub(crate) fn keep_local(
        &self,
        back: &Arc<super::recv::Returns>,
        item: super::recv::Back,
    ) -> bool {
        self.recv
            .try_borrow_mut()
            .is_ok_and(|mut group| group.as_mut().is_some_and(|g| g.keep_local(back, item)))
    }

    /// The group id multishot receives draw from, if the ring has a group.
    pub(crate) fn recv_group(&self) -> Option<u16> {
        self.recv.borrow().as_ref().map(RecvGroup::bgid)
    }

    /// Whether a stream send may go out zero-copy on this ring: one a keeper can take, so a
    /// buffer the kernel still holds as the shard goes comes back once it let go of it.
    pub(crate) fn zc_stream(&self) -> bool {
        self.send_zc && self.reapable && !self.zc_denied.get()
    }

    /// The least a zero-copy write must have left for its next send to go zero-copy here.
    pub(crate) fn zero_copy_min(&self) -> usize {
        self.zero_copy_min.get()
    }

    /// Sets [`zero_copy_min`](Self::zero_copy_min), once, as the worker's shard is made.
    pub(crate) fn set_zero_copy_min(&self, bytes: usize) {
        self.zero_copy_min.set(bytes);
    }

    /// Whether another thread can submit to the ring and reap its completions: a keeper can take
    /// it as its shard goes (see `keeper`).
    pub(crate) fn reapable(&self) -> bool {
        self.reapable
    }

    /// Whether the ring's group is filled in pieces, which a datagram cannot share: the kernel
    /// would cut it to the room left in a buffer.
    pub(crate) fn recv_group_incremental(&self) -> bool {
        self.recv
            .borrow()
            .as_ref()
            .is_some_and(RecvGroup::incremental)
    }

    /// The size of the group's buffers, if the ring has a group.
    pub(crate) fn recv_buf_size(&self) -> Option<u32> {
        self.recv.borrow().as_ref().map(RecvGroup::buf_size)
    }

    /// The memory the ring's group takes (0 without one), charged to the shard's ledger.
    pub(crate) fn recv_reservation(&self) -> usize {
        self.recv
            .borrow()
            .as_ref()
            .map_or(0, RecvGroup::reservation)
    }

    pub(crate) fn notifier(&self) -> Notifier {
        self.poller.borrow().notifier()
    }

    /// Submits `op` with its entry `sqe` (whose pointers point into `op`'s
    /// memory, which lives on the heap and does not move with it), on the
    /// slot `op.file`, and returns its user data. It goes into the kernel at
    /// once, never counted against the cap; its slot is named from now on.
    #[inline]
    pub(crate) fn submit(&self, op: RingOp, sqe: Sqe) -> u64 {
        let sqe = match self.files.borrow_mut().name(op.file) {
            Some(own) => sqe.fd(own),
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
        let write = counts_as_write(&op);
        let inserted = self.ops.borrow_mut().insert(op);
        let user_data = match inserted {
            Ok(user_data) => user_data,
            // Past 2^32 - 2 operations at once (two per source at most).
            Err(op) => {
                self.end(None, op, -libc::ENOBUFS);
                return IGNORED;
            }
        };
        if write {
            self.writes.set(self.writes.get() + 1);
        }
        self.enter(user_data, sqe, false);
        user_data
    }

    /// Puts the entry of the operation of `user_data` into the kernel, or
    /// behind the ones waiting for room. One that waits on a peer or a child
    /// (`counted` false) goes in at once and is not counted.
    pub(super) fn enter(&self, user_data: u64, sqe: Sqe, counted: bool) {
        let sqe = sqe.user_data(user_data);
        if !counted {
            self.poller.borrow_mut().push(sqe);
        } else if self.room.borrow().is_empty() && self.in_kernel.get() < self.cap {
            self.in_kernel.set(self.in_kernel.get() + 1);
            self.poller.borrow_mut().push(sqe);
        } else {
            self.room.borrow_mut().push_back((user_data, sqe));
        }
    }

    /// Submits a file's read or write, on its own file (no slot of the
    /// table): the operation's lease keeps the descriptor open until it
    /// ends. With the kernel full it waits for room.
    pub(crate) fn submit_file(&self, mut op: FileOp) {
        #[cfg(test)]
        self.file_calls.set(self.file_calls.get() + 1);
        let sqe = op.entry();
        let inserted = self.file_ops.borrow_mut().insert(op);
        let user_data = match inserted {
            Ok(user_data) => user_data | FILE_TAG,
            // Past 2^30 file calls at once: the call goes to the pool.
            Err(op) => {
                drop(op.complete(-libc::ECANCELED));
                return;
            }
        };
        self.enter(user_data, sqe, true);
    }

    /// A file's call completed: the rest of a write goes on, else its end
    /// is left for its task.
    fn complete_file(&self, user_data: u64, res: i32) {
        let Some(op) = self.file_ops.borrow_mut().remove(user_data & !FILE_TAG) else {
            return;
        };
        self.in_kernel.set(self.in_kernel.get().saturating_sub(1));
        self.admit_waiting();
        if let Some(rest) = op.complete(res) {
            self.submit_file(rest);
        }
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

    /// Cancels the source's operation of `user_data`, which is in the kernel:
    /// its completion says what it did. Returns whether a cancel went to the
    /// kernel's queue.
    pub(crate) fn cancel(&self, user_data: u64) -> bool {
        if self.ops.borrow_mut().get_mut(user_data).is_some() {
            self.poller
                .borrow_mut()
                .push(Sqe::async_cancel(user_data).user_data(IGNORED));
            return true;
        }
        false
    }

    /// A slot holding the file of `fd`, which the caller keeps open for
    /// the call: one of the ring's table, or past a full table, one naming
    /// `fd` itself, which its source keeps open for as long as an entry
    /// waits to name it (no copy is made: a process with no descriptor to
    /// spare registers all the same).
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
            None => Ok(self.files.borrow_mut().take_own(fd)),
        }
    }

    /// The source of `slot` went (closed, retired, evicted or lingering
    /// on): its sides' operations `out` are cancelled, and with every entry
    /// naming the slot handed to the kernel now, the file is taken out of
    /// the slot now (a fixed file out of the table; `lease`, which keeps the
    /// source's object open, let go), so the kernel closes it as soon as its
    /// requests let go of it, which the cancels see to before this returns
    /// (proofs/tla/ring/RingClose.tla). The slot stays reserved until no
    /// entry names it. Where the kernel took not every entry (it held
    /// completions it had no room for), this is finished after the shard's
    /// next wait.
    pub(crate) fn detach(&self, slot: u32, out: [Option<u64>; 2], lease: Option<Lease>) {
        let mut cancelled = false;
        for user_data in out.into_iter().flatten() {
            cancelled |= self.cancel(user_data);
        }
        // Entries naming the slot, or cancels of them, may still wait in
        // the submission queue: they go to the kernel now, and the work the
        // kernel holds for this thread runs, so the cancelled requests let
        // go of their files before this returns.
        let named = cancelled || self.files.borrow().is_named(slot);
        if named && !self.poller.borrow_mut().submit_now().unwrap_or(false) {
            // Still held, so the slot is not handed out meanwhile.
            self.leaving.borrow_mut().push((slot, lease));
            return;
        }
        self.take_out(slot, lease);
    }

    /// The source of `slot` went and no entry naming the slot waits to reach
    /// the kernel: a fixed file leaves the table, the slot is released (free
    /// once its entries completed), and the source's object may close.
    fn take_out(&self, slot: u32, lease: Option<Lease>) {
        if self.files.borrow().is_fixed(slot) {
            // A failure leaves the file in the slot, reserved, until its next
            // source replaces it.
            let _ = self.poller.borrow().update_file(slot, -1);
        }
        self.files.borrow_mut().release(slot);
        drop(lease);
    }

    /// Slots of sources that went, waiting for their entries to reach the
    /// kernel: once every queued entry did, they are taken out.
    fn finish_leaving(&self) {
        if self.leaving.borrow().is_empty()
            || !self.poller.borrow_mut().submit_now().unwrap_or(false)
        {
            return;
        }
        let leaving = core::mem::take(&mut *self.leaving.borrow_mut());
        for (slot, lease) in leaving {
            self.take_out(slot, lease);
        }
    }

    /// Submits what is queued and waits up to `timeout` for completions,
    /// which are handed to their sources. Returns how many it handled and
    /// whether a wake came.
    ///
    /// `shard` is the shard whose thread calls, when it is at hand: the
    /// sources then do not look it up for each completion.
    pub(crate) fn wait(
        &self,
        shard: Option<&Shard>,
        timeout: Option<Duration>,
    ) -> io::Result<(usize, bool)> {
        self.take_back();
        let mut batch = CqeBatch::new();
        let mut woken = self.poller.borrow_mut().wait(timeout, &mut batch);
        let mut handled = 0;
        loop {
            handled += batch.len();
            for at in 0..batch.len() {
                self.complete(shard, batch.get(at));
            }
            // A full batch may have left more in the queue.
            if !batch.is_full() {
                break;
            }
            batch.clear();
            let (woke_more, _) = self.poller.borrow_mut().reap(&mut batch);
            woken = woken.map(|w| w || woke_more);
        }
        self.finish_leaving();
        Ok((handled, woken?))
    }

    fn complete(&self, shard: Option<&Shard>, cqe: IoUringCqe) {
        if cqe.user_data & FILE_TAG != 0 {
            if cqe.user_data & EXIT_TAG != 0 {
                self.complete_exit(cqe.user_data, cqe.res);
            } else {
                self.complete_file(cqe.user_data, cqe.res);
            }
            return;
        }
        // The common end, one lookup: a plain operation's only completion.
        let plain =
            if cqe.flags & (IORING_CQE_F_MORE | IORING_CQE_F_NOTIF | IORING_CQE_F_BUFFER) == 0 {
                self.ops.borrow_mut().remove_if(cqe.user_data, ends_plain)
            } else {
                None
            };
        if let Some(op) = plain {
            if counts_as_write(&op) {
                self.writes.set(self.writes.get().saturating_sub(1));
            }
            self.end(shard, op, cqe.res);
            return;
        }
        if let Some(bid) = cqe_buffer_id(cqe.flags) {
            self.arrived(
                shard,
                cqe.user_data,
                bid,
                cqe.res,
                cqe.flags & IORING_CQE_F_BUF_MORE != 0,
            );
        } else if self.zero_copy(shard, &cqe) {
            return;
        } else if cqe.res >= 0 {
            self.accepted(shard, cqe.user_data, cqe.res);
        }
        // More of a multishot receive's arrivals come: it stays in flight.
        if cqe.flags & IORING_CQE_F_MORE != 0 {
            return;
        }
        let Some(op) = self.ops.borrow_mut().remove(cqe.user_data) else {
            return;
        };
        if counts_as_write(&op) {
            self.writes.set(self.writes.get().saturating_sub(1));
        }
        self.end(shard, op, cqe.res);
    }

    /// A completion of a zero-copy send, handled here; false for any other. The first completion,
    /// with the bytes the kernel took, ends the send for its side at once, which goes on to its
    /// next chunk or write; the send then stays in the ring only to hold the buffer the kernel may
    /// still read, until its notification (`IORING_CQE_F_NOTIF`), and no longer names its slot or
    /// counts among the write sides' operations, since it waits on the network. A first
    /// completion without `IORING_CQE_F_MORE` (a send refused before it ran) has no notification
    /// after it. The kernel posts the notification after the first completion; one that comes
    /// before it is kept for it. `proofs/tla/lent/LentZc.tla` checks the rule.
    fn zero_copy(&self, shard: Option<&Shard>, cqe: &IoUringCqe) -> bool {
        let notif = cqe.flags & IORING_CQE_F_NOTIF != 0;
        let mut ops = self.ops.borrow_mut();
        let Some(op) = ops.get_mut(cqe.user_data) else {
            return false;
        };
        let waits = match &mut op.mem {
            RingMem::SendZc { notified, .. } if notif => {
                *notified = true;
                return true;
            }
            RingMem::SendZc { notified, .. } => cqe.flags & IORING_CQE_F_MORE != 0 && !*notified,
            RingMem::ZcHeld(_) if notif => {
                // The kernel let go of what the send took: so does the send, and the last hold
                // hands the buffer back.
                let held = ops.remove(cqe.user_data);
                drop(ops);
                #[cfg(test)]
                self.zc_seen
                    .set((self.zc_seen.get().0, self.zc_seen.get().1 + 1));
                drop(held);
                return true;
            }
            _ => return false,
        };
        let (source, file, epoch) = (Arc::clone(&op.source), op.file, op.epoch);
        let ended = if waits {
            op.mem.hold_for_notification();
            None
        } else {
            ops.remove(cqe.user_data)
        };
        drop(ops);
        #[cfg(test)]
        if waits {
            self.zc_seen
                .set((self.zc_seen.get().0 + 1, self.zc_seen.get().1));
        }
        self.writes.set(self.writes.get().saturating_sub(1));
        let mut res = cqe.res;
        if zc_refused(res) {
            // No memory to pin: nothing was sent, and the chunk goes again with a copy.
            self.zc_denied.set(true);
            res = -libc::ECANCELED;
        }
        let outcome = crate::io::source::ring::zc_outcome(res);
        self.hand_over(shard, &source, file, epoch, false, outcome);
        drop(ended);
        true
    }

    /// Buffer `bid` of the receive group came with `res` bytes for the
    /// multishot receive of `user_data`: its source gets them as a chunk.
    /// Past the cap of bytes it holds, the receive is stopped; its source
    /// asks for another once its reader took them.
    fn arrived(&self, shard: Option<&Shard>, user_data: u64, bid: u16, res: i32, more: bool) {
        let mut stop = false;
        let delivery = {
            let mut recv = self.recv.borrow_mut();
            let Some(group) = recv.as_mut() else {
                return;
            };
            let mut ops = self.ops.borrow_mut();
            let held = ops.get_mut(user_data).and_then(|op| {
                let source = std::sync::Arc::as_ptr(&op.source);
                match &mut op.mem {
                    RingMem::RecvMulti { held, stopping } => {
                        Some((op.epoch, held, stopping, false, source))
                    }
                    RingMem::RecvFromMulti { held, stopping, .. } => {
                        Some((op.epoch, held, stopping, true, source))
                    }
                    _ => None,
                }
            });
            match (u32::try_from(res), held) {
                (Ok(len @ 1..), Some((epoch, held, stopping, datagram, source))) => {
                    match group.hand_out(user_data, bid, len, more) {
                        Some(chunk) => {
                            *held += chunk.charged(group);
                            if *held > group.cap() && !*stopping {
                                *stopping = true;
                                stop = true;
                            }
                            Some((source, chunk, epoch, datagram))
                        }
                        // An id or a length outside the group: the buffer goes back.
                        None => {
                            group.give_back(bid, len, more);
                            None
                        }
                    }
                }
                // No bytes, or no receive to give them to: the buffer goes
                // back at once.
                _ => {
                    group.give_back(bid, u32::try_from(res).unwrap_or(0), more);
                    None
                }
            }
        };
        if stop {
            self.cancel(user_data);
        }
        // Handed over with nothing borrowed: the source may submit at once.
        if let Some((source, chunk, epoch, datagram)) = delivery {
            // Before the view, so it drops after it.
            let _unwound = crate::io::side::Unwound::enter();
            let mut after = After::default();
            let starved = {
                // SAFETY: the op's slab entry owns a strong count of the
                // source until its final completion, which this thread
                // processes only after this call returns. Until the view is
                // dropped below nothing runs foreign code or removes the
                // entry: a cancel only queues a cancel entry (a multishot
                // receive never waits for room), and the shard's shutdown
                // runs on this thread outside any source code. The wakes
                // run after the view is gone. The view is never dropped as
                // an `Arc`.
                let source =
                    core::mem::ManuallyDrop::new(unsafe { std::sync::Arc::from_raw(source) });
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
                source.ring_arrival(arrival, epoch, shard, &mut after);
                after.starved.then(|| std::sync::Arc::clone(&source))
            };
            if let Some(source) = starved {
                source.wait_for_buffer();
            }
            after.run();
        }
    }

    /// A completion `res` of the operation of `user_data` that carries no
    /// buffer: for a multishot accept, a connection (its descriptor), handed
    /// to the source as it comes. Past the cap of connections one accept
    /// hands over, the accept is stopped; its source asks for another once
    /// its tasks took them.
    fn accepted(&self, shard: Option<&Shard>, user_data: u64, res: i32) {
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
        let mut after = After::default();
        source.ring_arrival(Arrival::Conn(conn), epoch, shard, &mut after);
        if after.starved {
            std::sync::Arc::clone(&source).wait_for_buffer();
        }
        after.run();
    }

    /// Takes in the buffers chunks gave back and gives them to the kernel
    /// again, counting each off the receive it went to.
    fn take_back(&self) {
        let mut recv = self.recv.borrow_mut();
        let Some(group) = recv.as_mut() else {
            return;
        };
        let mut ops = self.ops.borrow_mut();
        group.take_back(|user_data, charged| {
            if let Some(RingMem::RecvMulti { held, .. } | RingMem::RecvFromMulti { held, .. }) =
                ops.get_mut(user_data).map(|op| &mut op.mem)
            {
                *held = held.saturating_sub(charged);
            }
        });
    }

    /// `op` ended with `res`: its slot is named once less, and its source
    /// gets the completion.
    fn end(&self, shard: Option<&Shard>, op: RingOp, res: i32) {
        let RingOp {
            source,
            file,
            epoch,
            read,
            mem,
        } = op;
        #[cfg(test)]
        if res == -libc::ENOBUFS && matches!(mem, RingMem::RecvMulti { .. }) {
            let (armed, arrived, dry) = self.counts.get();
            self.counts.set((armed, arrived, dry + 1));
        }
        let outcome = mem.complete(res, source.kind());
        self.hand_over(shard, &source, file, epoch, read, outcome);
    }

    /// An operation of `source` on slot `file`, issued in `epoch` for its read side (`read`) or
    /// its write side, ended for that side with `outcome`: the slot is named once less, and the
    /// side gets the completion.
    #[inline]
    fn hand_over(
        &self,
        shard: Option<&Shard>,
        source: &Arc<crate::io::source::Source>,
        file: u32,
        epoch: u32,
        read: bool,
        outcome: crate::io::source::ops::Outcome,
    ) {
        // The slot's file left the table as its source went: a slot that
        // comes free only goes back to its list.
        self.files.borrow_mut().done(file);
        source.op_done(
            Done {
                outcome,
                epoch,
                read,
            },
            shard,
        );
    }
}

impl ShardRing {
    /// Operations in flight: the sources', the files' and the children's.
    pub(crate) fn in_flight(&self) -> usize {
        self.ops.borrow().len() + self.file_ops.borrow().len() + self.exit_ops.borrow().len()
    }

    /// Times the kernel began holding completions the ring's queue had no
    /// room for.
    pub(crate) fn overflows(&self) -> u64 {
        self.poller.borrow().overflows()
    }
}

/// Whether the kernel refused a zero-copy send: no memory it may pin, or a socket that has none.
fn zc_refused(res: i32) -> bool {
    [
        -libc::ENOBUFS,
        -libc::ENOMEM,
        -libc::EPERM,
        -libc::EOPNOTSUPP,
    ]
    .contains(&res)
}

/// How many buffers the receive group of a ring with `cq_entries`
/// completion slots gets when the configuration asks for `configured`: no
/// more than the largest power of two within half the queue, so arrivals
/// never fill it; none when the queue cannot hold one. A count that is not a
/// power of two is passed on unchanged, for the group to refuse as the
/// configuration's mistake.
fn group_entries(configured: u16, cq_entries: u32) -> u16 {
    if configured == 0 || !configured.is_power_of_two() {
        return configured;
    }
    let half = cq_entries / 2;
    if half == 0 {
        return 0;
    }
    let fit = 1u32 << half.ilog2();
    configured.min(u16::try_from(fit.min(1 << 15)).unwrap_or(1 << 15))
}

/// Whether `op` is one of the write sides' operations counted in `writes`: a
/// write side's, until a zero-copy send only waits for its notification. The
/// one place both the submission and the end of an operation decide it.
/// Whether `op` ends at its one completion, with no arrival, notification or follow-up to wait for.
fn ends_plain(op: &RingOp) -> bool {
    !matches!(
        op.mem,
        RingMem::SendZc { .. }
            | RingMem::ZcHeld(_)
            | RingMem::RecvMulti { .. }
            | RingMem::RecvFromMulti { .. }
            | RingMem::AcceptMulti { .. }
    )
}

fn counts_as_write(op: &RingOp) -> bool {
    !op.read && !matches!(op.mem, RingMem::ZcHeld(_))
}

/// The bit of the user data of a file's call (in the slot index, which the
/// sources' operations never reach).
const FILE_TAG: u64 = 1 << 31;
/// With [`FILE_TAG`], the bit of the user data of a child's exit watch (in
/// the slot index, which a file's call never reaches).
const EXIT_TAG: u64 = 1 << 30;

/// The connections one multishot accept hands over before the ring stops
/// it: its source asks for another once its tasks took them.
const ACCEPT_CAP: usize = 256;

/// The user data of the ring's own cancels, whose completions say nothing
/// to anyone: no operation of the slab has it (its index is the one the
/// slab never hands out), and it is not the wake's.
const IGNORED: u64 = u32::MAX as u64;

#[cfg(test)]
impl ShardRing {
    /// Slots held or still named, and of them the ones past the table (which
    /// name their sources' own descriptors).
    pub(crate) fn files_in_use(&self) -> (usize, usize) {
        let files = self.files.borrow();
        (files.in_use(), files.own_in_use())
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

    /// Chunks of the group handed out and not given back yet, and whether the group is filled in
    /// pieces.
    pub(crate) fn live_chunks(&self) -> (u32, bool) {
        self.take_back();
        self.recv
            .borrow()
            .as_ref()
            .map_or((0, false), |g| (g.live(), g.incremental()))
    }

    /// Zero-copy sends' first completions and notifications seen so far.
    pub(crate) fn zc_seen(&self) -> (usize, usize) {
        self.zc_seen.get()
    }

    /// Files' calls submitted so far.
    pub(crate) fn file_calls(&self) -> usize {
        self.file_calls.get()
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

    /// The buffers the receive group has.
    pub(crate) fn group_buffers(&self) -> Option<u16> {
        self.recv.borrow().as_ref().map(RecvGroup::entries)
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
/// sources can be open while it holds, and the kernel refuses a larger table
/// with `EMFILE`), at most [`MAX_TABLE`], whose slots cost kernel memory per
/// ring. Sources past it run on their own descriptors; every source does
/// where the kernel lets go of a file out of the table only late (see
/// `files::release_at_once`).
pub(crate) fn table_len() -> u32 {
    #[cfg(test)]
    if let Some(len) = TABLE_FOR_TEST.with(Cell::get) {
        return len;
    }
    if !crate::syscore::linux::iouring::files::release_at_once() {
        return 0;
    }
    // SAFETY: plain data; the call fills it.
    let mut limit: libc::rlimit = unsafe { core::mem::zeroed() };
    // SAFETY: a plain syscall writing `limit`.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        1024
    };
    u32::try_from(soft).unwrap_or(MAX_TABLE).min(MAX_TABLE)
}

/// The most fixed files a ring's table holds.
const MAX_TABLE: u32 = 1 << 14;

/// The fewest submission entries a ring short of memory is made with.
const MIN_RING_ENTRIES: u32 = crate::config::MIN_QUEUE_LEN;

/// The fixed files, and buffers of a receive group, [`check`] makes: the
/// features are the kernel's to give, whatever the sizes.
const CHECK_TABLE: u32 = 64;
const CHECK_BUFFERS: u16 = 2;

/// Whether the kernel gives a shard of `cfg` everything its ring needs: the
/// ring with its setup flags and disabled start, a table of fixed files, a
/// group of buffers for multishot receives, and the enabling by the thread
/// that runs it. It makes the ring as a shard does ([`ShardRing::new`]) but
/// at the smallest sizes that prove each feature, then drops it (nothing is
/// in flight in a ring that never waited), so the backend choice sees every
/// feature a shard needs and never disagrees with the shard, at the cost of
/// a small ring instead of a shard's.
pub(crate) fn check(cfg: RingConfig) -> io::Result<()> {
    let group = cfg.recv_buffers != 0;
    let small = RingConfig {
        entries: MIN_RING_ENTRIES,
        cq_entries: None,
        recv_buffers: cfg.recv_buffers.min(CHECK_BUFFERS),
        recv_buffer_size: if group {
            crate::config::MIN_RECV_BUFFER_SIZE
        } else {
            cfg.recv_buffer_size
        },
        ..cfg
    };
    let files = table_len().min(CHECK_TABLE);
    ShardRing::new(&small, files)?.enable()
}

pub(crate) mod exits;
pub(crate) mod keeper;
#[cfg(test)]
pub(crate) use keeper::leaked;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_cap;
#[cfg(test)]
mod tests_inc;
#[cfg(test)]
mod tests_recv;
#[cfg(test)]
mod tests_reserve;
#[cfg(test)]
mod tests_zc;
