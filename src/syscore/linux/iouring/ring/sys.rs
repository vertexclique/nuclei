//! The io_uring kernel ABI and the raw syscalls behind it.
//!
//! Every struct is a `#[repr(C)]` mirror of the one in `linux/io_uring.h` with its size and the
//! offsets the ring layer touches pinned by compile-time asserts; every constant carries the
//! header's own name and value. Nothing here owns ring state: the wrappers translate a failed
//! syscall into an [`io::Error`] and hand everything else back untouched (`EINTR` included, so
//! the caller decides whether to retry). [`Mapping`] is the one owner of an `mmap` region.

use std::{
    ffi::c_void,
    io,
    mem::{offset_of, size_of},
    os::fd::{FromRawFd, OwnedFd, RawFd},
    ptr::{self, NonNull},
    time::Duration,
};

/// Submission queue entry: 64 bytes, with the header's unions flattened to the fields in use.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoUringSqe {
    /// Operation, an [`Opcode`] value.
    pub opcode: u8,
    /// `IOSQE_*` flags.
    pub flags: u8,
    /// Per-operation flags (`ioprio` for reads and writes, socket flags for accept, recv, send).
    pub ioprio: u16,
    /// File descriptor the operation works on.
    pub fd: i32,
    /// File offset, or `addr2` for operations that take a second address.
    pub off: u64,
    /// Buffer or iovec address, or `splice_off_in`.
    pub addr: u64,
    /// Buffer length or iovec count.
    pub len: u32,
    /// The per-operation flags union (`rw_flags`, `accept_flags`, `timeout_flags`, ...).
    pub op_flags: u32,
    /// Opaque value echoed in the completion.
    pub user_data: u64,
    /// Fixed buffer index, or the provided-buffer group.
    pub buf_index: u16,
    /// Registered credentials id.
    pub personality: u16,
    /// Fixed file slot, or `splice_fd_in`.
    pub file_index: u32,
    /// Third address (`addr3`).
    pub addr3: u64,
    /// Padding reserved by the kernel.
    pub pad: u64,
}

/// Completion queue entry: 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct IoUringCqe {
    /// The `user_data` of the submission this completes.
    pub user_data: u64,
    /// Result: a byte count or fd on success, a negated errno on failure.
    pub res: i32,
    /// `IORING_CQE_F_*` flags, the provided-buffer id in the top 16 bits.
    pub flags: u32,
}

/// Where the kernel placed the submission ring fields, as offsets into the ring mapping.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoSqringOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

/// Where the kernel placed the completion ring fields, as offsets into the ring mapping.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoCqringOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

/// In/out argument of `io_uring_setup`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoUringParams {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: IoSqringOffsets,
    pub cq_off: IoCqringOffsets,
}

/// Extended argument of `io_uring_enter` (`IORING_ENTER_EXT_ARG`): signal mask and wait timeout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoUringGeteventsArg {
    pub sigmask: u64,
    pub sigmask_sz: u32,
    pub min_wait_usec: u32,
    /// Address of a [`KernelTimespec`], or 0 for no timeout.
    pub ts: u64,
}

/// Fixed head of the `IORING_REGISTER_PROBE` reply; the op array follows it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoUringProbe {
    pub last_op: u8,
    pub ops_len: u8,
    pub resv: u16,
    pub resv2: [u32; 3],
}

/// One entry of the probe reply.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoUringProbeOp {
    pub op: u8,
    pub resv: u8,
    /// `IO_URING_OP_SUPPORTED` when the kernel implements the op.
    pub flags: u16,
    pub resv2: u32,
}

/// One provided-buffer ring entry; entry 0's `resv` doubles as the ring tail.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoUringBuf {
    pub addr: u64,
    pub len: u32,
    pub bid: u16,
    pub resv: u16,
}

/// Argument of `IORING_REGISTER_PBUF_RING` and `IORING_UNREGISTER_PBUF_RING`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoUringBufReg {
    pub ring_addr: u64,
    pub ring_entries: u32,
    pub bgid: u16,
    pub flags: u16,
    pub min_left: u32,
    pub resv: [u32; 5],
}

/// Header the kernel writes in front of a multishot `recvmsg` payload.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct IoUringRecvmsgOut {
    pub namelen: u32,
    pub controllen: u32,
    pub payloadlen: u32,
    pub flags: u32,
}

/// Argument of `IORING_REGISTER_RING_FDS`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IoUringRsrcUpdate {
    pub offset: u32,
    pub resv: u32,
    pub data: u64,
}

/// The kernel's 64-bit timespec (`__kernel_timespec`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct KernelTimespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

const _: () = {
    assert!(size_of::<IoUringSqe>() == 64);
    assert!(offset_of!(IoUringSqe, fd) == 4);
    assert!(offset_of!(IoUringSqe, off) == 8);
    assert!(offset_of!(IoUringSqe, addr) == 16);
    assert!(offset_of!(IoUringSqe, len) == 24);
    assert!(offset_of!(IoUringSqe, op_flags) == 28);
    assert!(offset_of!(IoUringSqe, user_data) == 32);
    assert!(offset_of!(IoUringSqe, buf_index) == 40);
    assert!(offset_of!(IoUringSqe, personality) == 42);
    assert!(offset_of!(IoUringSqe, file_index) == 44);
    assert!(offset_of!(IoUringSqe, addr3) == 48);

    assert!(size_of::<IoUringCqe>() == 16);
    assert!(offset_of!(IoUringCqe, res) == 8);
    assert!(offset_of!(IoUringCqe, flags) == 12);

    assert!(size_of::<IoSqringOffsets>() == 40);
    assert!(offset_of!(IoSqringOffsets, flags) == 16);
    assert!(offset_of!(IoSqringOffsets, array) == 24);
    assert!(size_of::<IoCqringOffsets>() == 40);
    assert!(offset_of!(IoCqringOffsets, overflow) == 16);
    assert!(offset_of!(IoCqringOffsets, cqes) == 20);

    assert!(size_of::<IoUringParams>() == 120);
    assert!(offset_of!(IoUringParams, flags) == 8);
    assert!(offset_of!(IoUringParams, sq_thread_idle) == 16);
    assert!(offset_of!(IoUringParams, features) == 20);
    assert!(offset_of!(IoUringParams, sq_off) == 40);
    assert!(offset_of!(IoUringParams, cq_off) == 80);

    assert!(size_of::<IoUringGeteventsArg>() == 24);
    assert!(offset_of!(IoUringGeteventsArg, sigmask_sz) == 8);
    assert!(offset_of!(IoUringGeteventsArg, min_wait_usec) == 12);
    assert!(offset_of!(IoUringGeteventsArg, ts) == 16);

    assert!(size_of::<IoUringProbe>() == 16);
    assert!(size_of::<IoUringProbeOp>() == 8);
    assert!(offset_of!(IoUringProbeOp, flags) == 2);

    assert!(size_of::<IoUringBuf>() == 16);
    assert!(offset_of!(IoUringBuf, len) == 8);
    assert!(offset_of!(IoUringBuf, bid) == 12);
    assert!(offset_of!(IoUringBuf, resv) == 14);

    assert!(size_of::<IoUringBufReg>() == 40);
    assert!(offset_of!(IoUringBufReg, ring_entries) == 8);
    assert!(offset_of!(IoUringBufReg, bgid) == 12);
    assert!(offset_of!(IoUringBufReg, flags) == 14);

    assert!(size_of::<IoUringRecvmsgOut>() == 16);
    assert!(offset_of!(IoUringRecvmsgOut, payloadlen) == 8);

    assert!(size_of::<IoUringRsrcUpdate>() == 16);
    assert!(offset_of!(IoUringRsrcUpdate, data) == 8);

    assert!(size_of::<KernelTimespec>() == 16);
    assert!(offset_of!(KernelTimespec, tv_nsec) == 8);
};

// io_uring_setup flags.
pub(crate) const IORING_SETUP_IOPOLL: u32 = 1 << 0;
pub(crate) const IORING_SETUP_SQPOLL: u32 = 1 << 1;
pub(crate) const IORING_SETUP_SQ_AFF: u32 = 1 << 2;
pub(crate) const IORING_SETUP_CQSIZE: u32 = 1 << 3;
pub(crate) const IORING_SETUP_CLAMP: u32 = 1 << 4;
pub(crate) const IORING_SETUP_ATTACH_WQ: u32 = 1 << 5;
pub(crate) const IORING_SETUP_R_DISABLED: u32 = 1 << 6;
pub(crate) const IORING_SETUP_SUBMIT_ALL: u32 = 1 << 7;
pub(crate) const IORING_SETUP_COOP_TASKRUN: u32 = 1 << 8;
pub(crate) const IORING_SETUP_TASKRUN_FLAG: u32 = 1 << 9;
pub(crate) const IORING_SETUP_SINGLE_ISSUER: u32 = 1 << 12;
pub(crate) const IORING_SETUP_DEFER_TASKRUN: u32 = 1 << 13;
pub(crate) const IORING_SETUP_NO_SQARRAY: u32 = 1 << 16;

// io_uring_enter flags.
pub(crate) const IORING_ENTER_GETEVENTS: u32 = 1 << 0;
pub(crate) const IORING_ENTER_SQ_WAKEUP: u32 = 1 << 1;
pub(crate) const IORING_ENTER_SQ_WAIT: u32 = 1 << 2;
pub(crate) const IORING_ENTER_EXT_ARG: u32 = 1 << 3;
pub(crate) const IORING_ENTER_REGISTERED_RING: u32 = 1 << 4;
pub(crate) const IORING_ENTER_NO_IOWAIT: u32 = 1 << 7;

// Submission ring flags.
pub(crate) const IORING_SQ_NEED_WAKEUP: u32 = 1 << 0;
pub(crate) const IORING_SQ_CQ_OVERFLOW: u32 = 1 << 1;
pub(crate) const IORING_SQ_TASKRUN: u32 = 1 << 2;

// io_uring_params.features flags.
pub(crate) const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;
pub(crate) const IORING_FEAT_NODROP: u32 = 1 << 1;
pub(crate) const IORING_FEAT_SUBMIT_STABLE: u32 = 1 << 2;
pub(crate) const IORING_FEAT_RW_CUR_POS: u32 = 1 << 3;
pub(crate) const IORING_FEAT_CUR_PERSONALITY: u32 = 1 << 4;
pub(crate) const IORING_FEAT_FAST_POLL: u32 = 1 << 5;
pub(crate) const IORING_FEAT_POLL_32BITS: u32 = 1 << 6;
pub(crate) const IORING_FEAT_SQPOLL_NONFIXED: u32 = 1 << 7;
pub(crate) const IORING_FEAT_EXT_ARG: u32 = 1 << 8;
pub(crate) const IORING_FEAT_NATIVE_WORKERS: u32 = 1 << 9;
pub(crate) const IORING_FEAT_RSRC_TAGS: u32 = 1 << 10;
pub(crate) const IORING_FEAT_CQE_SKIP: u32 = 1 << 11;
pub(crate) const IORING_FEAT_LINKED_FILE: u32 = 1 << 12;
pub(crate) const IORING_FEAT_REG_REG_RING: u32 = 1 << 13;
pub(crate) const IORING_FEAT_RECVSEND_BUNDLE: u32 = 1 << 14;
pub(crate) const IORING_FEAT_MIN_TIMEOUT: u32 = 1 << 15;
pub(crate) const IORING_FEAT_RW_ATTR: u32 = 1 << 16;
pub(crate) const IORING_FEAT_NO_IOWAIT: u32 = 1 << 17;

// sqe.flags.
pub(crate) const IOSQE_FIXED_FILE: u8 = 1 << 0;
pub(crate) const IOSQE_IO_DRAIN: u8 = 1 << 1;
pub(crate) const IOSQE_IO_LINK: u8 = 1 << 2;
pub(crate) const IOSQE_IO_HARDLINK: u8 = 1 << 3;
pub(crate) const IOSQE_ASYNC: u8 = 1 << 4;
pub(crate) const IOSQE_BUFFER_SELECT: u8 = 1 << 5;
pub(crate) const IOSQE_CQE_SKIP_SUCCESS: u8 = 1 << 6;

// cqe.flags.
pub(crate) const IORING_CQE_F_BUFFER: u32 = 1 << 0;
pub(crate) const IORING_CQE_F_MORE: u32 = 1 << 1;
pub(crate) const IORING_CQE_F_SOCK_NONEMPTY: u32 = 1 << 2;
pub(crate) const IORING_CQE_F_NOTIF: u32 = 1 << 3;
pub(crate) const IORING_CQE_BUFFER_SHIFT: u32 = 16;

// io_uring_register opcodes.
pub(crate) const IORING_REGISTER_PROBE: u32 = 8;
pub(crate) const IORING_REGISTER_ENABLE_RINGS: u32 = 12;
pub(crate) const IORING_REGISTER_IOWQ_MAX_WORKERS: u32 = 19;
pub(crate) const IORING_REGISTER_RING_FDS: u32 = 20;
pub(crate) const IORING_UNREGISTER_RING_FDS: u32 = 21;
pub(crate) const IORING_REGISTER_PBUF_RING: u32 = 22;
pub(crate) const IORING_UNREGISTER_PBUF_RING: u32 = 23;

// mmap offsets of the ring regions.
pub(crate) const IORING_OFF_SQ_RING: u64 = 0;
pub(crate) const IORING_OFF_CQ_RING: u64 = 0x800_0000;
pub(crate) const IORING_OFF_SQES: u64 = 0x1000_0000;

// sqe.ioprio flags of accept, recv and send.
pub(crate) const IORING_ACCEPT_MULTISHOT: u16 = 1 << 0;
pub(crate) const IORING_RECVSEND_POLL_FIRST: u16 = 1 << 0;
pub(crate) const IORING_RECV_MULTISHOT: u16 = 1 << 1;

// sqe.cancel_flags.
pub(crate) const IORING_ASYNC_CANCEL_ALL: u32 = 1 << 0;
pub(crate) const IORING_ASYNC_CANCEL_FD: u32 = 1 << 1;
pub(crate) const IORING_ASYNC_CANCEL_ANY: u32 = 1 << 2;

// sqe.timeout_flags.
pub(crate) const IORING_TIMEOUT_ABS: u32 = 1 << 0;
pub(crate) const IORING_TIMEOUT_BOOTTIME: u32 = 1 << 2;
pub(crate) const IORING_TIMEOUT_REALTIME: u32 = 1 << 3;
pub(crate) const IORING_TIMEOUT_ETIME_SUCCESS: u32 = 1 << 5;

/// sqe.len flag of `IORING_OP_POLL_ADD`: keep the poll armed after it fires.
pub(crate) const IORING_POLL_ADD_MULTI: u32 = 1 << 0;
/// sqe.addr command of `IORING_OP_MSG_RING`: pass `len` and `off` to the target ring as a CQE.
pub(crate) const IORING_MSG_DATA: u64 = 0;
/// sqe.fsync_flags: flush data only, not metadata.
pub(crate) const IORING_FSYNC_DATASYNC: u32 = 1 << 0;
/// `io_uring_probe_op.flags` bit set when the kernel implements the op.
pub(crate) const IO_URING_OP_SUPPORTED: u16 = 1 << 0;

/// The ring operations, in the numbering of the header's `enum io_uring_op`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Opcode {
    Nop,
    Readv,
    Writev,
    Fsync,
    ReadFixed,
    WriteFixed,
    PollAdd,
    PollRemove,
    SyncFileRange,
    Sendmsg,
    Recvmsg,
    Timeout,
    TimeoutRemove,
    Accept,
    AsyncCancel,
    LinkTimeout,
    Connect,
    Fallocate,
    Openat,
    Close,
    FilesUpdate,
    Statx,
    Read,
    Write,
    Fadvise,
    Madvise,
    Send,
    Recv,
    Openat2,
    EpollCtl,
    Splice,
    ProvideBuffers,
    RemoveBuffers,
    Tee,
    Shutdown,
    Renameat,
    Unlinkat,
    Mkdirat,
    Symlinkat,
    Linkat,
    MsgRing,
    Fsetxattr,
    Setxattr,
    Fgetxattr,
    Getxattr,
    Socket,
    UringCmd,
    SendZc,
    SendmsgZc,
    ReadMultishot,
    Waitid,
    FutexWait,
    FutexWake,
    FutexWaitv,
    FixedFdInstall,
    Ftruncate,
    Bind,
    Listen,
    RecvZc,
    EpollWait,
    ReadvFixed,
    WritevFixed,
    Pipe,
    Nop128,
    UringCmd128,
}

/// Buffer id carried in the top 16 bits of the CQE flags, when the CQE used a provided buffer.
pub(crate) const fn cqe_buffer_id(flags: u32) -> Option<u16> {
    if flags & IORING_CQE_F_BUFFER != 0 {
        Some((flags >> IORING_CQE_BUFFER_SHIFT) as u16)
    } else {
        None
    }
}

/// The opcodes a probe reported as implemented: a bitset over the 256 possible opcodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpcodeSet([u64; 4]);

impl OpcodeSet {
    /// Marks the raw opcode `op` as supported.
    pub(crate) fn insert(&mut self, op: u8) {
        self.0[usize::from(op) / 64] |= 1 << (op % 64);
    }

    /// True when the kernel implements `op`.
    pub(crate) fn contains(&self, op: Opcode) -> bool {
        let op = op as u8;
        self.0[usize::from(op) / 64] & (1 << (op % 64)) != 0
    }
}

/// Number of op slots a probe reply can carry (the opcode is a `u8`).
pub(crate) const PROBE_OPS: usize = 256;

/// Probe reply buffer: the fixed head followed by room for every possible op.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ProbeBuf {
    pub head: IoUringProbe,
    pub ops: [IoUringProbeOp; PROBE_OPS],
}

impl Default for ProbeBuf {
    fn default() -> Self {
        Self {
            head: IoUringProbe::default(),
            ops: [IoUringProbeOp::default(); PROBE_OPS],
        }
    }
}

impl KernelTimespec {
    /// Converts a duration, saturating the seconds at the largest value the kernel can hold.
    pub(crate) fn from_duration(d: Duration) -> Self {
        Self {
            tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(d.subsec_nanos()),
        }
    }
}

/// The parts of a multishot `recvmsg` completion buffer, split by [`IoUringRecvmsgOut::parse`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RecvmsgParts<'a> {
    /// The header the kernel wrote.
    pub header: IoUringRecvmsgOut,
    /// Source address bytes, at most the `msg_namelen` that was requested.
    pub name: &'a [u8],
    /// Control message bytes, at most the `msg_controllen` that was requested.
    pub control: &'a [u8],
    /// The received payload.
    pub payload: &'a [u8],
}

impl IoUringRecvmsgOut {
    /// Splits the buffer of a multishot `recvmsg` completion (`buf` is the first `cqe.res` bytes of
    /// the selected buffer; `msg_namelen` and `msg_controllen` are the values the submitted msghdr
    /// carried). `None` when the buffer is shorter than the layout it claims.
    pub(crate) fn parse(
        buf: &[u8],
        msg_namelen: u32,
        msg_controllen: u32,
    ) -> Option<RecvmsgParts<'_>> {
        const HDR: usize = size_of::<IoUringRecvmsgOut>();
        let word = |i: usize| -> Option<u32> {
            let b = buf.get(i * 4..i * 4 + 4)?;
            Some(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
        };
        let header = Self {
            namelen: word(0)?,
            controllen: word(1)?,
            payloadlen: word(2)?,
            flags: word(3)?,
        };
        let name_end = HDR.checked_add(usize::try_from(msg_namelen).ok()?)?;
        let control_end = name_end.checked_add(usize::try_from(msg_controllen).ok()?)?;
        if buf.len() < control_end {
            return None;
        }
        let name_len = (header.namelen as usize).min(msg_namelen as usize);
        let control_len = (header.controllen as usize).min(msg_controllen as usize);
        let payload = &buf[control_end..];
        Some(RecvmsgParts {
            header,
            name: &buf[HDR..HDR + name_len],
            control: &buf[name_end..name_end + control_len],
            payload: &payload[..payload.len().min(header.payloadlen as usize)],
        })
    }
}

/// `io_uring_setup(2)`: creates a ring and returns its fd; the kernel fills `params` in.
pub(crate) fn io_uring_setup(entries: u32, params: &mut IoUringParams) -> io::Result<OwnedFd> {
    // SAFETY: `params` is a valid, exclusively borrowed `io_uring_params` for the whole call.
    let r = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            entries as libc::c_long,
            ptr::from_mut(params),
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `io_uring_setup` returns a new fd that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(r as RawFd) })
}

/// `io_uring_enter(2)`; returns what the kernel returned. `EINTR` is an error the caller handles.
///
/// # Safety
/// `arg` must be null or point to `argsz` readable bytes holding what `flags` implies (an
/// [`IoUringGeteventsArg`] with `IORING_ENTER_EXT_ARG`), and everything that argument points to
/// must stay valid for the call.
pub(crate) unsafe fn io_uring_enter(
    fd: RawFd,
    to_submit: u32,
    min_complete: u32,
    flags: u32,
    arg: *const c_void,
    argsz: usize,
) -> io::Result<u32> {
    // SAFETY: the caller upholds the contract on `arg` and `argsz`.
    let r = unsafe {
        libc::syscall(
            libc::SYS_io_uring_enter,
            fd as libc::c_long,
            to_submit as libc::c_long,
            min_complete as libc::c_long,
            flags as libc::c_long,
            arg,
            argsz,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r as u32)
}

/// `io_uring_register(2)`; returns what the kernel returned.
///
/// # Safety
/// `arg` must be valid for the `opcode`: null, or pointing to `nr_args` readable (and, for
/// opcodes that reply, writable) elements of the structure the opcode expects.
pub(crate) unsafe fn io_uring_register(
    fd: RawFd,
    opcode: u32,
    arg: *const c_void,
    nr_args: u32,
) -> io::Result<u32> {
    // SAFETY: the caller upholds the contract on `arg` and `nr_args`.
    let r = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            fd as libc::c_long,
            opcode as libc::c_long,
            arg,
            nr_args as libc::c_long,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r as u32)
}

/// An owned `mmap` region, unmapped on drop.
#[derive(Debug)]
pub(crate) struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapping {
    fn map(len: usize, flags: libc::c_int, fd: RawFd, offset: u64) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot map zero bytes",
            ));
        }
        // SAFETY: a null hint lets the kernel pick the address; the result is checked below and
        // then owned by the returned `Mapping`.
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                offset as libc::off_t,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(p.cast::<u8>())
            .ok_or_else(|| io::Error::other("the kernel mapped memory at address zero"))?;
        Ok(Self { ptr, len })
    }

    /// Maps `len` bytes of a ring region (`offset` is one of the `IORING_OFF_*` values).
    pub(crate) fn ring(fd: RawFd, len: usize, offset: u64) -> io::Result<Self> {
        Self::map(len, libc::MAP_SHARED | libc::MAP_POPULATE, fd, offset)
    }

    /// Maps `len` bytes of zeroed, page-aligned anonymous memory, faulted in on first use.
    pub(crate) fn anon(len: usize) -> io::Result<Self> {
        Self::map(len, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
    }

    /// Start of the region.
    pub(crate) fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Length of the region in bytes.
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `len` describe a live mapping this value owns exclusively.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}
