//! Submission entry builders: one safe constructor per supported operation.
//!
//! A builder only fills in the 64-byte entry; it never touches the memory it points at. Every
//! builder that takes a raw pointer (a buffer, an iovec, a path, a sockaddr, a msghdr, a
//! timespec, an output struct) leaves the caller responsible for keeping that memory valid and
//! unmoved until the operation's final completion arrives: for a multishot operation that is
//! the completion without `IORING_CQE_F_MORE`, and for a cancelled one the completion of the
//! cancelled operation itself, not of the cancel request.

use std::{ffi::c_char, os::fd::RawFd};

use super::sys::{
    IORING_ACCEPT_MULTISHOT, IORING_ASYNC_CANCEL_ALL, IORING_ASYNC_CANCEL_ANY,
    IORING_ASYNC_CANCEL_FD, IORING_ASYNC_CANCEL_FD_FIXED, IORING_FSYNC_DATASYNC, IORING_MSG_DATA,
    IORING_POLL_ADD_MULTI, IORING_RECV_MULTISHOT, IORING_TIMEOUT_ABS, IOSQE_BUFFER_SELECT,
    IoUringSqe, KernelTimespec, Opcode,
};

/// Address of `p` as the kernel expects it in an entry field.
fn addr<T>(p: *const T) -> u64 {
    p.expose_provenance() as u64
}

/// One submission queue entry under construction.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub(crate) struct Sqe(pub(super) IoUringSqe);

impl Sqe {
    fn new(op: Opcode, fd: RawFd) -> Self {
        Self(IoUringSqe {
            opcode: op as u8,
            fd,
            ..IoUringSqe::default()
        })
    }

    fn with(mut self, fill: impl FnOnce(&mut IoUringSqe)) -> Self {
        fill(&mut self.0);
        self
    }

    /// Sets the value echoed back in this operation's completion(s).
    /// The entry as the kernel reads it, for a test.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &IoUringSqe {
        &self.0
    }

    pub(crate) fn user_data(mut self, user_data: u64) -> Self {
        self.0.user_data = user_data;
        self
    }

    /// Adds `IOSQE_*` flags (links, drain, async, ...) to the entry.
    pub(crate) fn flags(mut self, flags: u8) -> Self {
        self.0.flags |= flags;
        self
    }

    /// Points the entry at descriptor (or fixed slot) `fd` instead.
    pub(crate) fn fd(mut self, fd: RawFd) -> Self {
        self.0.fd = fd;
        self
    }

    /// Adds per-operation `ioprio` bits (the multishot and poll-first flags of the socket ops).
    pub(crate) fn ioprio(mut self, ioprio: u16) -> Self {
        self.0.ioprio |= ioprio;
        self
    }

    /// Sets the provided-buffer group the operation selects from (pair with `IOSQE_BUFFER_SELECT`).
    pub(crate) fn buf_group(mut self, bgid: u16) -> Self {
        self.0.buf_index = bgid;
        self
    }

    /// A no-op that completes with result 0.
    pub(crate) fn nop() -> Self {
        Self::new(Opcode::Nop, -1)
    }

    /// Reads up to `len` bytes into `buf` at file `offset` (`u64::MAX` reads at the current position).
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn read(fd: RawFd, buf: *mut u8, len: u32, offset: u64) -> Self {
        Self::new(Opcode::Read, fd).with(|s| {
            s.addr = addr(buf);
            s.len = len;
            s.off = offset;
        })
    }

    /// Writes `len` bytes from `buf` at file `offset` (`u64::MAX` writes at the current position).
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn write(fd: RawFd, buf: *const u8, len: u32, offset: u64) -> Self {
        Self::new(Opcode::Write, fd).with(|s| {
            s.addr = addr(buf);
            s.len = len;
            s.off = offset;
        })
    }

    /// Scatter read into `count` iovecs at `iov`, at file `offset`. The iovec array and the buffers
    /// it names must both stay valid and unmoved until the final completion.
    pub(crate) fn readv(fd: RawFd, iov: *const libc::iovec, count: u32, offset: u64) -> Self {
        Self::new(Opcode::Readv, fd).with(|s| {
            s.addr = addr(iov);
            s.len = count;
            s.off = offset;
        })
    }

    /// Gather write from `count` iovecs at `iov`, at file `offset`. The iovec array and the buffers
    /// it names must both stay valid and unmoved until the final completion.
    pub(crate) fn writev(fd: RawFd, iov: *const libc::iovec, count: u32, offset: u64) -> Self {
        Self::new(Opcode::Writev, fd).with(|s| {
            s.addr = addr(iov);
            s.len = count;
            s.off = offset;
        })
    }

    /// `openat(2)` of the NUL-terminated `path` relative to `dirfd`; completes with the new fd.
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn openat(dirfd: RawFd, path: *const c_char, flags: i32, mode: u32) -> Self {
        Self::new(Opcode::Openat, dirfd).with(|s| {
            s.addr = addr(path);
            s.len = mode;
            s.op_flags = flags as u32;
        })
    }

    /// `close(2)` of `fd`.
    pub(crate) fn close(fd: RawFd) -> Self {
        Self::new(Opcode::Close, fd)
    }

    /// `statx(2)` of the NUL-terminated `path` relative to `dirfd`, filling `out`.
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn statx(
        dirfd: RawFd,
        path: *const c_char,
        flags: i32,
        mask: u32,
        out: *mut libc::statx,
    ) -> Self {
        Self::new(Opcode::Statx, dirfd).with(|s| {
            s.addr = addr(path);
            s.len = mask;
            s.off = addr(out);
            s.op_flags = flags as u32;
        })
    }

    /// `fsync(2)`, or `fdatasync(2)` when `datasync` is set.
    pub(crate) fn fsync(fd: RawFd, datasync: bool) -> Self {
        Self::new(Opcode::Fsync, fd).with(|s| {
            s.op_flags = if datasync { IORING_FSYNC_DATASYNC } else { 0 };
        })
    }

    /// `accept4(2)`; completes with the new fd. `peer` and `peer_len` may be null.
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn accept(
        fd: RawFd,
        peer: *mut libc::sockaddr,
        peer_len: *mut libc::socklen_t,
        flags: i32,
    ) -> Self {
        Self::new(Opcode::Accept, fd).with(|s| {
            s.addr = addr(peer);
            s.off = addr(peer_len);
            s.op_flags = flags as u32;
        })
    }

    /// Multishot `accept4(2)`: one completion per connection, `IORING_CQE_F_MORE` while armed.
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn accept_multishot(
        fd: RawFd,
        peer: *mut libc::sockaddr,
        peer_len: *mut libc::socklen_t,
        flags: i32,
    ) -> Self {
        Self::accept(fd, peer, peer_len, flags).ioprio(IORING_ACCEPT_MULTISHOT)
    }

    /// `connect(2)` to the address of `addr_len` bytes at `peer`.
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn connect(
        fd: RawFd,
        peer: *const libc::sockaddr,
        addr_len: libc::socklen_t,
    ) -> Self {
        Self::new(Opcode::Connect, fd).with(|s| {
            s.addr = addr(peer);
            s.off = u64::from(addr_len);
        })
    }

    /// `send(2)` of `len` bytes from `buf`.
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn send(fd: RawFd, buf: *const u8, len: u32, flags: i32) -> Self {
        Self::new(Opcode::Send, fd).with(|s| {
            s.addr = addr(buf);
            s.len = len;
            s.op_flags = flags as u32;
        })
    }

    /// Zero-copy `send(2)` of `len` bytes from `buf`: the first completion carries the byte count
    /// and `IORING_CQE_F_MORE`, and a second one with `IORING_CQE_F_NOTIF` says the kernel is done
    /// with the memory. The notification follows a failed send too (its first completion carries
    /// the error with `MORE`); a first completion without `MORE` is a send refused before it ran,
    /// and none follows.
    /// Every pointer argument must stay valid and unmoved until the notification.
    pub(crate) fn send_zc(fd: RawFd, buf: *const u8, len: u32, flags: i32) -> Self {
        Self::new(Opcode::SendZc, fd).with(|s| {
            s.addr = addr(buf);
            s.len = len;
            s.op_flags = flags as u32;
        })
    }

    /// `recv(2)` of up to `len` bytes into `buf`.
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn recv(fd: RawFd, buf: *mut u8, len: u32, flags: i32) -> Self {
        Self::new(Opcode::Recv, fd).with(|s| {
            s.addr = addr(buf);
            s.len = len;
            s.op_flags = flags as u32;
        })
    }

    /// One-shot `recv(2)` into a buffer the kernel picks from group `bgid`; the completion carries
    /// the buffer id. `len` caps the bytes taken (0 means the whole buffer).
    pub(crate) fn recv_select(fd: RawFd, bgid: u16, len: u32, flags: i32) -> Self {
        Self::new(Opcode::Recv, fd)
            .with(|s| {
                s.len = len;
                s.op_flags = flags as u32;
            })
            .flags(IOSQE_BUFFER_SELECT)
            .buf_group(bgid)
    }

    /// Multishot `recv(2)` into buffers picked from group `bgid`: one completion per receive,
    /// `IORING_CQE_F_MORE` while armed. It ends without `MORE` on error or when the group runs dry.
    pub(crate) fn recv_multishot(fd: RawFd, bgid: u16, flags: i32) -> Self {
        Self::recv_select(fd, bgid, 0, flags).ioprio(IORING_RECV_MULTISHOT)
    }

    /// `sendmsg(2)` of the message at `msg`. The msghdr, its iovecs, name and control buffers must
    /// all stay valid and unmoved until the final completion.
    pub(crate) fn sendmsg(fd: RawFd, msg: *const libc::msghdr, flags: u32) -> Self {
        Self::new(Opcode::Sendmsg, fd).with(|s| {
            s.addr = addr(msg);
            s.len = 1;
            s.op_flags = flags;
        })
    }

    /// `recvmsg(2)` into the message at `msg`. The msghdr, its iovecs, name and control buffers must
    /// all stay valid and unmoved until the final completion.
    pub(crate) fn recvmsg(fd: RawFd, msg: *mut libc::msghdr, flags: u32) -> Self {
        Self::new(Opcode::Recvmsg, fd).with(|s| {
            s.addr = addr(msg);
            s.len = 1;
            s.op_flags = flags;
        })
    }

    /// Multishot `recvmsg(2)` into buffers picked from group `bgid`. Only `msg_namelen` and
    /// `msg_controllen` of the msghdr are read; each completion's buffer holds an
    /// `io_uring_recvmsg_out`, the name, the control data and the payload (see
    /// `IoUringRecvmsgOut::parse`). The msghdr must stay valid and unmoved until the final completion.
    pub(crate) fn recvmsg_multishot(
        fd: RawFd,
        msg: *const libc::msghdr,
        bgid: u16,
        flags: u32,
    ) -> Self {
        Self::new(Opcode::Recvmsg, fd)
            .with(|s| {
                s.addr = addr(msg);
                s.len = 1;
                s.op_flags = flags;
            })
            .flags(IOSQE_BUFFER_SELECT)
            .buf_group(bgid)
            .ioprio(IORING_RECV_MULTISHOT)
    }

    /// `shutdown(2)` of `fd` (`how` is `SHUT_RD`, `SHUT_WR` or `SHUT_RDWR`).
    pub(crate) fn shutdown(fd: RawFd, how: i32) -> Self {
        Self::new(Opcode::Shutdown, fd).with(|s| s.len = how as u32)
    }

    /// `socket(2)`; completes with the new fd.
    pub(crate) fn socket(domain: i32, ty: i32, protocol: i32, flags: u32) -> Self {
        Self::new(Opcode::Socket, domain).with(|s| {
            s.off = ty as u64;
            s.len = protocol as u32;
            s.op_flags = flags;
        })
    }

    /// Timeout at `ts` with `IORING_TIMEOUT_*` `flags`. It completes with `-ETIME` when the
    /// time passes, or with 0 after `count` other completions when `count` is non-zero. The
    /// timespec at `ts` must stay valid and unmoved until the final completion.
    pub(crate) fn timeout_with(ts: *const KernelTimespec, count: u32, flags: u32) -> Self {
        Self::new(Opcode::Timeout, -1).with(|s| {
            s.addr = addr(ts);
            s.len = 1;
            s.off = u64::from(count);
            s.op_flags = flags;
        })
    }

    /// Relative timeout: `-ETIME` after the interval at `ts`, which must stay valid and unmoved
    /// until the final completion.
    pub(crate) fn timeout(ts: *const KernelTimespec) -> Self {
        Self::timeout_with(ts, 0, 0)
    }

    /// Absolute timeout: `-ETIME` at the instant `ts` on the monotonic clock, or on the clock
    /// `clock_flags` names (`IORING_TIMEOUT_BOOTTIME` or `IORING_TIMEOUT_REALTIME`). The timespec
    /// at `ts` must stay valid and unmoved until the final completion.
    pub(crate) fn timeout_abs(ts: *const KernelTimespec, clock_flags: u32) -> Self {
        Self::timeout_with(ts, 0, IORING_TIMEOUT_ABS | clock_flags)
    }

    /// Removes the pending timeout whose `user_data` is `target`; the target completes with
    /// `-ECANCELED` and this entry with 0.
    pub(crate) fn timeout_remove(target: u64) -> Self {
        Self::new(Opcode::TimeoutRemove, -1).with(|s| s.addr = target)
    }

    /// Cancels the pending operation whose `user_data` is `target`: the target completes with
    /// `-ECANCELED` and this entry with 0 (or `-ENOENT` when nothing matched).
    pub(crate) fn async_cancel(target: u64) -> Self {
        Self::new(Opcode::AsyncCancel, -1).with(|s| s.addr = target)
    }

    /// Cancels the pending operations on `fd` (just the first match, or every match with `all`);
    /// this entry completes with the number cancelled.
    pub(crate) fn async_cancel_fd(fd: RawFd, all: bool) -> Self {
        Self::new(Opcode::AsyncCancel, fd).with(|s| {
            s.op_flags = IORING_ASYNC_CANCEL_FD | if all { IORING_ASYNC_CANCEL_ALL } else { 0 };
        })
    }

    /// Cancels the pending operations on fixed file `slot` (the first found, or every one with
    /// `all`).
    pub(crate) fn async_cancel_fixed(slot: u32, all: bool) -> Self {
        Self::new(Opcode::AsyncCancel, slot as RawFd).with(|s| {
            s.op_flags = IORING_ASYNC_CANCEL_FD
                | IORING_ASYNC_CANCEL_FD_FIXED
                | if all { IORING_ASYNC_CANCEL_ALL } else { 0 };
        })
    }

    /// Cancels any pending operation (the first found, or every one with `all`).
    pub(crate) fn async_cancel_any(all: bool) -> Self {
        Self::new(Opcode::AsyncCancel, -1).with(|s| {
            s.op_flags = IORING_ASYNC_CANCEL_ANY | if all { IORING_ASYNC_CANCEL_ALL } else { 0 };
        })
    }

    /// One-shot poll of `fd` for the `poll(2)` event mask `events`.
    pub(crate) fn poll_add(fd: RawFd, events: u32) -> Self {
        Self::new(Opcode::PollAdd, fd).with(|s| {
            // The kernel reads the mask word-swapped on big-endian machines.
            s.op_flags = if cfg!(target_endian = "big") {
                events.rotate_left(16)
            } else {
                events
            };
        })
    }

    /// Multishot poll: one completion per event, `IORING_CQE_F_MORE` while armed.
    pub(crate) fn poll_add_multi(fd: RawFd, events: u32) -> Self {
        Self::poll_add(fd, events).with(|s| s.len = IORING_POLL_ADD_MULTI)
    }

    /// Removes the pending poll whose `user_data` is `target`.
    pub(crate) fn poll_remove(target: u64) -> Self {
        Self::new(Opcode::PollRemove, -1).with(|s| s.addr = target)
    }

    /// Posts a completion to another ring (`target_ring_fd`) with `user_data` `target_user_data`
    /// and result `res`; this entry completes in the sending ring with 0.
    pub(crate) fn msg_ring(target_ring_fd: RawFd, target_user_data: u64, res: u32) -> Self {
        Self::new(Opcode::MsgRing, target_ring_fd).with(|s| {
            s.addr = IORING_MSG_DATA;
            s.off = target_user_data;
            s.len = res;
        })
    }

    /// `waitid(2)` on `id` of kind `idtype` (`P_PID`, ...) with wait `options`, filling `info`.
    /// Every pointer argument must stay valid and unmoved until the final completion.
    pub(crate) fn waitid(
        idtype: libc::idtype_t,
        id: libc::id_t,
        info: *mut libc::siginfo_t,
        options: u32,
    ) -> Self {
        Self::new(Opcode::Waitid, id as RawFd).with(|s| {
            // `idtype_t` is unsigned on glibc and musl, signed on Bionic; the
            // kernel reads the same bits.
            s.len = u32::from_ne_bytes(idtype.to_ne_bytes());
            s.off = addr(info);
            s.file_index = options;
        })
    }
}
