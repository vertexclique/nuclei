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
//!
//! The group is sized to the ring: at most half its completion queue, so a
//! small ring (made so under memory pressure) is never filled by arrivals.
//! Multishot receives and accepts wait on the network for as long as their
//! connection lives, so they are let into the kernel at once and do not
//! count against the cap that finite operations wait behind: a sleeping
//! connection would otherwise hold the room a request's send needs. The
//! receives' arrivals are bounded by the group's buffers, which the queue
//! keeps room for. An accept's connections (up to [`ACCEPT_CAP`] each) and
//! the last completion of each multishot operation have no room kept: past
//! the queue the kernel holds them (`IORING_FEAT_NODROP`, which the poller
//! requires) until the poller takes them, and none is lost.

use core::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::time::Duration;

use super::Shard;
use super::recv::RecvGroup;
use crate::fs::ring::FileOp;
use crate::io::source::ops::Done;
use crate::io::source::ring::{Arrival, RingMem, RingOp};
use crate::io::sys;
use crate::rt::backend::with_room;
use crate::rt::park::Notifier;
use crate::rt::park::ring::RingPoller;
use crate::syscore::linux::iouring::files::Files;
use crate::syscore::linux::iouring::ops::Ops;
use crate::syscore::linux::iouring::ring::sys::{
    IORING_CQE_F_BUF_MORE, IORING_CQE_F_MORE, IORING_CQE_F_NOTIF, IOSQE_FIXED_FILE, cqe_buffer_id,
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
    files: RefCell<Files>,
    /// Operations submitted but not in the kernel yet, by user data, with
    /// their entries, in order.
    room: RefCell<VecDeque<(u64, Sqe)>>,
    /// Finite operations in the kernel (see [`counts_against_cap`]), and the
    /// most it may hold.
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
                // Short of memory: each read takes a buffer of the pool.
                Err(err) if err.raw_os_error() == Some(libc::ENOMEM) => None,
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
            files: RefCell::new(Files::new(files)),
            room: RefCell::new(VecDeque::new()),
            in_kernel: Cell::new(0),
            cap,
            cqes: RefCell::new(Vec::with_capacity(256)),
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

    /// Whether finite operations are out (in the kernel or waiting for room): their completions
    /// come within a moment. A multishot receive or accept waits on a peer for as long as it
    /// lives, so it is no sign of one.
    pub(crate) fn expects_completions(&self) -> bool {
        self.in_kernel.get() > 0 || !self.room.borrow().is_empty()
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

    /// Whether a stream send may go out zero-copy on this ring.
    pub(crate) fn zc_stream(&self) -> bool {
        self.send_zc && !self.zc_denied.get()
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
        let counted = counts_against_cap(&op.mem);
        let inserted = self.ops.borrow_mut().insert(op);
        let user_data = match inserted {
            Ok(user_data) => user_data,
            // Past 2^32 - 2 operations at once (two per source at most).
            Err(op) => {
                self.end(None, op, -libc::ENOBUFS);
                return IGNORED;
            }
        };
        self.enter(user_data, sqe, counted);
        user_data
    }

    /// Puts the entry of the operation of `user_data` into the kernel, or
    /// behind the ones waiting for room. A long-lived operation (`counted`
    /// false) goes in at once and is not counted.
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
    /// table): the operation holds a copy of the descriptor until it ends.
    /// With the kernel full it waits for room.
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
                self.end(None, op, -libc::ECANCELED);
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
    ///
    /// `shard` is the shard whose thread calls, when it is at hand: the
    /// sources then do not look it up for each completion.
    pub(crate) fn wait(
        &self,
        shard: Option<&Shard>,
        timeout: Option<Duration>,
    ) -> io::Result<(usize, bool)> {
        self.take_back();
        let mut cqes = core::mem::take(&mut *self.cqes.borrow_mut());
        let woken = self.poller.borrow_mut().wait(timeout, &mut cqes);
        let handled = cqes.len();
        for cqe in cqes.drain(..) {
            self.complete(shard, cqe);
        }
        *self.cqes.borrow_mut() = cqes;
        Ok((handled, woken?))
    }

    fn complete(&self, shard: Option<&Shard>, mut cqe: IoUringCqe) {
        if cqe.user_data & FILE_TAG != 0 {
            if cqe.user_data & EXIT_TAG != 0 {
                self.complete_exit(cqe.user_data, cqe.res);
            } else {
                self.complete_file(cqe.user_data, cqe.res);
            }
            return;
        }
        if self.zc_first(&cqe) {
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
        if op.mem.is_zc() {
            #[cfg(test)]
            self.zc_seen
                .set((self.zc_seen.get().0, self.zc_seen.get().1 + 1));
            // The notification: the memory is free, and the send ended as its
            // first completion said (a send refused before it ran has no
            // notification, and says it at once).
            cqe.res = op.mem.zc_result().unwrap_or(cqe.res);
            if zc_refused(cqe.res) {
                // No memory to pin: nothing was sent, and the send goes again with a copy.
                self.zc_denied.set(true);
                cqe.res = -libc::ECANCELED;
            }
        }
        if counts_against_cap(&op.mem) {
            self.in_kernel.set(self.in_kernel.get().saturating_sub(1));
            self.admit_waiting();
        }
        self.end(shard, op, cqe.res);
    }

    /// The first completion of a zero-copy send, which says how many bytes went and that a
    /// notification follows (`IORING_CQE_F_MORE`): kept for the notification, which ends the
    /// send once the kernel is done with its memory. True when `cqe` was such a first completion.
    fn zc_first(&self, cqe: &IoUringCqe) -> bool {
        if cqe.flags & IORING_CQE_F_MORE == 0 || cqe.flags & IORING_CQE_F_NOTIF != 0 {
            return false;
        }
        let kept = self
            .ops
            .borrow_mut()
            .get_mut(cqe.user_data)
            .is_some_and(|op| op.mem.zc_first(cqe.res));
        #[cfg(test)]
        if kept {
            self.zc_seen
                .set((self.zc_seen.get().0 + 1, self.zc_seen.get().1));
        }
        kept
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
            let (after, starved) = {
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
                let after = source.ring_arrival(arrival, epoch, shard);
                let starved = after.starved.then(|| std::sync::Arc::clone(&source));
                (after, starved)
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
        let after = source.ring_arrival(Arrival::Conn(conn), epoch, shard);
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
        #[cfg(test)]
        if res == -libc::ENOBUFS && matches!(op.mem, RingMem::RecvMulti { .. }) {
            let (armed, arrived, dry) = self.counts.get();
            self.counts.set((armed, arrived, dry + 1));
        }
        let freed = self.files.borrow_mut().done(op.file);
        self.freed(op.file, freed);
        let kind = op.source.kind();
        op.source.op_done(
            Done {
                outcome: op.mem.complete(res, kind),
                epoch: op.epoch,
                read: op.read,
            },
            shard,
        );
    }

    /// The shard is going, its sources detached: every operation still out
    /// is cancelled and its completion waited for, for at most
    /// [`SHUTDOWN_WAIT`]. One that never completes keeps its memory for
    /// good (the kernel may still write it), and is counted (`leaked`).
    pub(crate) fn shut_down(&self) {
        let waiting: Vec<u64> = self.room.borrow_mut().drain(..).map(|(u, _)| u).collect();
        for user_data in waiting {
            // Never submitted: nothing points at its memory. A file's call
            // goes to the pool; a child goes to be watched elsewhere.
            if user_data & (FILE_TAG | EXIT_TAG) == FILE_TAG | EXIT_TAG {
                self.drop_waiting_exit(user_data);
            } else if user_data & FILE_TAG != 0 {
                let op = self.file_ops.borrow_mut().remove(user_data & !FILE_TAG);
                if let Some(op) = op {
                    drop(op.complete(-libc::ECANCELED));
                }
            } else {
                let op = self.ops.borrow_mut().remove(user_data);
                if let Some(op) = op {
                    self.end(None, op, -libc::ECANCELED);
                }
            }
        }
        if self.in_flight() > 0 {
            self.poller
                .borrow_mut()
                .push(Sqe::async_cancel_any(true).user_data(IGNORED));
        }
        self.settle(|| {});
        let stuck: Vec<RingOp> = self.ops.borrow_mut().drain().collect();
        let stuck_files: Vec<FileOp> = self.file_ops.borrow_mut().drain().collect();
        let stuck_exits: Vec<ExitOp> = self.exit_ops.borrow_mut().drain().collect();
        LEAKED.fetch_add(
            stuck.len() + stuck_files.len() + stuck_exits.len(),
            core::sync::atomic::Ordering::Relaxed,
        );
        // A file's call still in the kernel keeps its buffer and its
        // descriptor for good, a child's wait its info and its pidfd.
        core::mem::forget(stuck_files);
        core::mem::forget(stuck_exits);
        if !stuck.is_empty() {
            // A receive still in the kernel may write the group's buffers:
            // they stay registered and mapped for good.
            core::mem::forget(self.recv.borrow_mut().take());
        }
        core::mem::forget(stuck);
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

    /// Submits what is queued and waits until no operation is out, for at
    /// most [`SHUTDOWN_WAIT`]: what its sources cancelled as they went.
    /// `between` runs before each wait (the shard's mailbox, where a
    /// source's holder on another thread leaves its cancel).
    pub(crate) fn settle(&self, mut between: impl FnMut()) {
        let deadline = std::time::Instant::now() + SHUTDOWN_WAIT;
        loop {
            between();
            if self.in_flight() == 0 {
                return;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero()
                || self
                    .wait(None, Some(left.min(Duration::from_millis(10))))
                    .is_err()
            {
                return;
            }
        }
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

/// Whether the operation of `mem` counts against the cap on operations in
/// the kernel. A multishot receive or accept waits on the network for as long
/// as its connection lives, so counting it would let idle connections fill
/// the cap and starve the sends that wake them. The one place both the
/// submission and the end of an operation decide it.
fn counts_against_cap(mem: &RingMem) -> bool {
    !matches!(
        mem,
        RingMem::RecvMulti { .. } | RingMem::RecvFromMulti { .. } | RingMem::AcceptMulti { .. }
    )
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
/// sources can be open while it holds), at most [`MAX_TABLE`], whose slots
/// cost kernel memory per ring. Sources past it run on copies; every source
/// does where the kernel lets go of a file out of the table only late (see
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
    u32::try_from(soft)
        .unwrap_or(MAX_TABLE)
        .clamp(64, MAX_TABLE)
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
    let files = if table_len() == 0 { 0 } else { CHECK_TABLE };
    ShardRing::new(&small, files)?.enable()
}

pub(crate) mod exits;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_cap;
#[cfg(test)]
mod tests_inc;
#[cfg(test)]
mod tests_recv;
#[cfg(test)]
mod tests_zc;
