//! The io_uring ring layer: raw syscalls, the shared rings and nothing else.
//!
//! [`Ring`] sets up one ring over `io_uring_setup`/`mmap`, pushes [`Sqe`]s into the submission
//! queue, enters the kernel to submit and wait, and hands completions back as copies. It owns
//! no operation state, no buffers a submission points at and no waker: what a pointer inside an
//! entry refers to is the caller's to keep valid until the operation's final completion, and a
//! `Ring` must be dropped only after the caller has drained its in-flight operations (the
//! operation slab of the layer above enforces that); the ring layer itself frees no memory the
//! kernel may still write into.
//!
//! The ring is built for one thread: it is created with `IORING_SETUP_SINGLE_ISSUER`, so only the
//! creating thread may submit to it, and [`Ring`] is neither `Send` nor `Sync` to match. Without
//! a kernel submission thread it also defers completion work to the submitter
//! (`IORING_SETUP_DEFER_TASKRUN`): completions that need task work appear only when the thread
//! enters the kernel, which [`Ring::submit`] and [`Ring::submit_and_wait`] always do for such a ring.

pub(crate) mod buf_ring;
pub(crate) mod queue;
pub(crate) mod sqe;
pub(crate) mod sys;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_net;
#[cfg(test)]
mod tests_setup;
#[cfg(test)]
mod tests_wake;

use std::{
    ffi::c_void,
    fmt, io,
    marker::PhantomData,
    mem::size_of,
    os::fd::{AsRawFd, OwnedFd, RawFd},
    ptr,
    time::Duration,
};

pub(crate) use queue::{Completions, SqFull};
pub(crate) use sqe::Sqe;
pub(crate) use sys::*;

use crate::config::IoUringConfiguration;
use queue::{Cq, Sq};

/// How a [`Ring`] is created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RingConfig {
    /// Submission queue entries (the kernel rounds up to a power of two and clamps to its limit).
    pub entries: u32,
    /// Completion queue entries; the ring always gets at least twice `entries`.
    pub cq_entries: Option<u32>,
    /// Run a kernel submission thread that sleeps after this many idle milliseconds.
    pub sqpoll_idle_ms: Option<u32>,
    /// Poll for completions instead of taking interrupts (files opened with `O_DIRECT` only).
    pub iopoll: bool,
    /// Create the ring disabled; [`Ring::enable`] turns it on.
    pub start_disabled: bool,
    /// Any thread may submit (one at a time): neither single-issuer nor with
    /// deferred task work, for a worker shard that may be handed to another
    /// thread.
    pub multi_issuer: bool,
    /// Worker thread limits for `[bounded, unbounded]` async work; 0 leaves a limit unchanged.
    pub iowq_max_workers: Option<[u32; 2]>,
    /// Buffers of the group multishot receives draw from (0: no group), and their size.
    pub recv_buffers: u16,
    pub recv_buffer_size: u32,
}

impl RingConfig {
    /// A ring of `entries` submission slots with every option off.
    pub(crate) fn new(entries: u32) -> Self {
        Self {
            entries,
            cq_entries: None,
            sqpoll_idle_ms: None,
            iopoll: false,
            start_disabled: false,
            multi_issuer: false,
            iowq_max_workers: None,
            recv_buffers: 0,
            recv_buffer_size: 0,
        }
    }

    /// This ring for a worker shard that may be handed to another thread: any thread may submit
    /// to it, one at a time.
    pub(crate) fn handed_over(self) -> Self {
        Self {
            multi_issuer: true,
            ..self
        }
    }
}

impl From<&IoUringConfiguration> for RingConfig {
    fn from(c: &IoUringConfiguration) -> Self {
        Self {
            entries: c.queue_len,
            cq_entries: None,
            sqpoll_idle_ms: c.sqpoll_wake_interval,
            iopoll: c.iopoll_enabled,
            start_disabled: false,
            multi_issuer: false,
            iowq_max_workers: (c.per_numa_bounded_worker_count.is_some()
                || c.per_numa_unbounded_worker_count.is_some())
            .then(|| {
                [
                    c.per_numa_bounded_worker_count.unwrap_or(0),
                    c.per_numa_unbounded_worker_count.unwrap_or(0),
                ]
            }),
            recv_buffers: c.recv_buffers,
            recv_buffer_size: c.recv_buffer_size,
        }
    }
}

/// The `io_uring_setup` flags for `cfg`, with or without the submission index array.
///
/// Without a kernel submission thread the ring is single-issuer with deferred task work; with one,
/// those two cannot be combined with it, so they are left out, and so they are for a ring any
/// thread may submit to (`multi_issuer`), whose completion work runs on the thread that submitted
/// the operation: at its next return from the kernel, interrupting it if it runs, waking it if it
/// sleeps. `NO_SQARRAY` needs Linux 6.6.
pub(crate) fn setup_flags(cfg: &RingConfig, no_sqarray: bool) -> u32 {
    let mut flags = IORING_SETUP_SUBMIT_ALL | IORING_SETUP_CLAMP | IORING_SETUP_CQSIZE;
    flags |= if cfg.sqpoll_idle_ms.is_some() {
        IORING_SETUP_SQPOLL
    } else if cfg.multi_issuer {
        0
    } else {
        IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_DEFER_TASKRUN
    };
    if cfg.iopoll {
        flags |= IORING_SETUP_IOPOLL;
    }
    if cfg.start_disabled {
        flags |= IORING_SETUP_R_DISABLED;
    }
    if no_sqarray {
        flags |= IORING_SETUP_NO_SQARRAY;
    }
    flags
}

pub(crate) fn setup_params(cfg: &RingConfig, no_sqarray: bool) -> IoUringParams {
    IoUringParams {
        flags: setup_flags(cfg, no_sqarray),
        cq_entries: cfg
            .cq_entries
            .unwrap_or(0)
            .max(cfg.entries.saturating_mul(2)),
        sq_thread_idle: cfg.sqpoll_idle_ms.unwrap_or(0),
        ..IoUringParams::default()
    }
}

/// Why the kernel cannot give this runtime a ring. It rides inside the `ErrorKind::Unsupported`
/// errors of [`Ring::new`], so the backend probe tells the cases apart without reading words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `io_uring_setup` answered `ENOSYS`.
    NotInKernel,
    /// The kernel refused the floor flags even without the submission index array.
    FloorFlags,
    /// The kernel's ring lacks a feature every ring requires, named in plain words.
    Feature(&'static str),
    /// The kernel refused to register a feature a shard's ring needs, named in plain words.
    Registration(&'static str),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInKernel => f.write_str("this kernel has no io_uring support"),
            Self::FloorFlags => {
                f.write_str("the kernel lacks the io_uring features this runtime needs")
            }
            Self::Feature(name) => write!(f, "the kernel's io_uring lacks {name}"),
            Self::Registration(name) => write!(f, "the kernel's io_uring refuses {name}"),
        }
    }
}

impl std::error::Error for Refusal {}

impl From<Refusal> for io::Error {
    fn from(r: Refusal) -> Self {
        io::Error::new(io::ErrorKind::Unsupported, r)
    }
}

/// Maps the kernel's refusal of a registration to [`Refusal::Registration`] of `name`, and passes
/// any other error through. The arguments of a registration are validated before the call, so an
/// `EINVAL` or `EOPNOTSUPP` from the kernel is its refusal, not the caller's mistake: a kernel
/// without the operation, or one that refuses valid input, as Ubuntu's 6.8 kernels from 6.8.0-136
/// on do for every provided buffer ring (a backport inverted the check of its reserved words,
/// <https://bugs.launchpad.net/ubuntu/+source/linux/+bug/2162843>). The two must stay apart so a refusal is never taken for a configuration
/// mistake.
pub(crate) fn refused(name: &'static str) -> impl FnOnce(io::Error) -> io::Error {
    move |e| match e.raw_os_error() {
        Some(libc::EINVAL | libc::EOPNOTSUPP) => Refusal::Registration(name).into(),
        _ => e,
    }
}

/// Runs `setup` with the preferred flags (no submission index array), then once more without
/// `NO_SQARRAY` if the kernel answers `EINVAL`. A second `EINVAL` means the kernel lacks the
/// features the ring needs. Returns the setup result and the parameters it succeeded with.
fn negotiate<T>(
    cfg: &RingConfig,
    mut setup: impl FnMut(u32, &mut IoUringParams) -> io::Result<T>,
) -> io::Result<(T, IoUringParams)> {
    for no_sqarray in [true, false] {
        let mut params = setup_params(cfg, no_sqarray);
        match setup(cfg.entries, &mut params) {
            Ok(v) => return Ok((v, params)),
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
                return Err(Refusal::NotInKernel.into());
            }
            Err(e) => return Err(e),
        }
    }
    Err(Refusal::FloorFlags.into())
}

/// Features every ring requires, with the plain-words name used when one is missing.
const REQUIRED_FEATURES: [(u32, &str); 4] = [
    (IORING_FEAT_SINGLE_MMAP, "a single mapping for both queues"),
    (IORING_FEAT_NODROP, "lossless completion overflow"),
    (IORING_FEAT_EXT_ARG, "timed waits"),
    (IORING_FEAT_FAST_POLL, "fast poll"),
];

/// The caller-facing result of an enter: an expired wait is not an error, and completions the
/// kernel had to drop are reported in words. Everything else, `EINTR` and `EBUSY` included, is
/// passed through for the caller to handle.
fn enter_outcome(r: io::Result<u32>) -> io::Result<u32> {
    match r {
        Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(0),
        Err(e) if e.raw_os_error() == Some(libc::EBADR) => Err(io::Error::other(
            "completions were lost because the completion queue overflowed",
        )),
        r => r,
    }
}

/// One io_uring instance: its fd, the two shared queues and the mappings behind them.
///
/// Drop it only after every operation in flight has completed or been cancelled and reaped: the
/// ring layer frees nothing the kernel may still write into. Fields drop in declaration order:
/// the fd closes first, then the mappings go away.
#[derive(Debug)]
pub(crate) struct Ring {
    fd: OwnedFd,
    sq: Sq,
    cq: Cq,
    flags: u32,
    features: u32,
    sq_entries: u32,
    cq_entries: u32,
    /// Index of the ring fd in this thread's registered table, once registered.
    reg_index: Option<u32>,
    /// Worker limits to apply when a disabled ring is enabled.
    deferred_iowq: Option<[u32; 2]>,
    _ring_map: sys::Mapping,
    _sqes_map: sys::Mapping,
    /// A single-issuer ring belongs to the thread that created it.
    _not_send: PhantomData<*const ()>,
}

impl Ring {
    /// Creates a ring as `cfg` describes (see [`setup_flags`] for the policy).
    ///
    /// Fails with `ErrorKind::Unsupported` when the kernel has no io_uring or lacks a feature the
    /// ring needs; the caller then falls back to another backend.
    pub(crate) fn new(cfg: &RingConfig) -> io::Result<Ring> {
        Self::with_setup(cfg, io_uring_setup)
    }

    /// [`Ring::new`] with the `io_uring_setup` call injected, so a test can drive the fallbacks.
    fn with_setup(
        cfg: &RingConfig,
        setup: impl FnMut(u32, &mut IoUringParams) -> io::Result<OwnedFd>,
    ) -> io::Result<Ring> {
        if cfg.entries == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the submission queue needs at least one entry",
            ));
        }
        let (fd, params) = negotiate(cfg, setup)?;
        for (bit, name) in REQUIRED_FEATURES {
            if params.features & bit == 0 {
                return Err(Refusal::Feature(name).into());
            }
        }
        if !params.sq_entries.is_power_of_two() || !params.cq_entries.is_power_of_two() {
            return Err(io::Error::other(
                "the kernel reported an unusable queue size",
            ));
        }

        let has_array = params.flags & IORING_SETUP_NO_SQARRAY == 0;
        let sq_len = if has_array {
            params.sq_off.array as usize + params.sq_entries as usize * size_of::<u32>()
        } else {
            0
        };
        let cq_len =
            params.cq_off.cqes as usize + params.cq_entries as usize * size_of::<IoUringCqe>();
        let sqes_len = params.sq_entries as usize * size_of::<IoUringSqe>();

        let ring_map = sys::Mapping::ring(fd.as_raw_fd(), sq_len.max(cq_len), IORING_OFF_SQ_RING)?;
        let sqes_map = sys::Mapping::ring(fd.as_raw_fd(), sqes_len, IORING_OFF_SQES)?;
        // SAFETY: both mappings were sized from the offsets the kernel returned, are owned by
        // the `Ring` built below next to these views, and drop after them.
        let (sq, cq) = unsafe {
            (
                Sq::new(
                    ring_map.as_ptr(),
                    &params.sq_off,
                    sqes_map.as_ptr().cast(),
                    params.sq_entries,
                    has_array,
                ),
                Cq::new(ring_map.as_ptr(), &params.cq_off, params.cq_entries),
            )
        };
        let mut ring = Ring {
            fd,
            sq,
            cq,
            flags: params.flags,
            features: params.features,
            sq_entries: params.sq_entries,
            cq_entries: params.cq_entries,
            reg_index: None,
            deferred_iowq: cfg.iowq_max_workers,
            _ring_map: ring_map,
            _sqes_map: sqes_map,
            _not_send: PhantomData,
        };
        if !cfg.start_disabled {
            ring.apply_deferred_iowq()?;
        }
        Ok(ring)
    }

    /// The raw ring fd.
    pub(crate) fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The `IORING_SETUP_*` flags the ring was created with.
    pub(crate) fn flags(&self) -> u32 {
        self.flags
    }

    /// The `IORING_FEAT_*` flags the kernel reported.
    pub(crate) fn features(&self) -> u32 {
        self.features
    }

    /// Submission queue size the kernel granted.
    pub(crate) fn sq_entries(&self) -> u32 {
        self.sq_entries
    }

    /// Completion queue size the kernel granted.
    pub(crate) fn cq_entries(&self) -> u32 {
        self.cq_entries
    }

    /// Free submission slots.
    pub(crate) fn sq_space(&self) -> u32 {
        self.sq.space()
    }

    /// Queues `sqe`; nothing reaches the kernel until [`Ring::submit`] or [`Ring::submit_and_wait`].
    pub(crate) fn push(&mut self, sqe: &Sqe) -> Result<(), SqFull> {
        self.sq.push(sqe)
    }

    /// Drains the completions that have arrived; see [`queue::Cq::completions`].
    pub(crate) fn completions(&mut self) -> Completions<'_> {
        self.cq.completions()
    }

    /// Whether the kernel holds completions that did not fit the completion queue. A
    /// [`Ring::submit_and_wait`] flushes them into the queue as space frees up.
    pub(crate) fn cq_overflow_pending(&self) -> bool {
        self.sq.cq_overflow_pending()
    }

    /// For a ring with a kernel submission thread: whether the thread has gone to sleep.
    pub(crate) fn sq_thread_asleep(&self) -> bool {
        self.sq.sqpoll_asleep()
    }

    /// Hands the queued entries to the kernel and returns how many it took (with a kernel
    /// submission thread: how many were made available to it).
    ///
    /// A ring that defers completion work to the submitter also runs that work here, so
    /// completions that are ready show up in the completion queue; the same holds for a polled
    /// ring and for a ring with overflowed completions. `EINTR` surfaces as
    /// `ErrorKind::Interrupted` and `EBUSY` means the completion backlog must be drained first.
    pub(crate) fn submit(&mut self) -> io::Result<u32> {
        let getevents = self.flags & (IORING_SETUP_DEFER_TASKRUN | IORING_SETUP_IOPOLL) != 0
            || self.cq_overflow_pending();
        self.enter(0, None, getevents)
    }

    /// Submits the queued entries, then waits until `min_complete` completions are available or
    /// `timeout` passes. A timeout that expires is not an error: the call returns `Ok` and the
    /// completions present, if any, are in the queue. Without a timeout the wait has no bound.
    ///
    /// Returns how many entries the kernel took (0 when none were queued). `EINTR` surfaces as
    /// `ErrorKind::Interrupted`. If the kernel dropped completions the call fails with a message
    /// saying so.
    pub(crate) fn submit_and_wait(
        &mut self,
        min_complete: u32,
        timeout: Option<Duration>,
    ) -> io::Result<u32> {
        self.enter(min_complete, timeout, true)
    }

    fn enter(
        &mut self,
        min_complete: u32,
        timeout: Option<Duration>,
        getevents: bool,
    ) -> io::Result<u32> {
        self.sq.publish();
        let queued = self.sq.ready();
        let mut flags = if getevents { IORING_ENTER_GETEVENTS } else { 0 };
        if self.flags & IORING_SETUP_SQPOLL != 0 {
            if self.sq.sqpoll_needs_wakeup() {
                flags |= IORING_ENTER_SQ_WAKEUP;
            }
            if flags == 0 {
                return Ok(queued);
            }
        } else if flags == 0 && queued == 0 {
            return Ok(0);
        }
        let (fd, reg_flag) = match self.reg_index {
            Some(i) => (i as RawFd, IORING_ENTER_REGISTERED_RING),
            None => (self.fd.as_raw_fd(), 0),
        };
        let ts = timeout
            .filter(|_| min_complete > 0)
            .map(KernelTimespec::from_duration);
        let arg = ts.as_ref().map(|ts| IoUringGeteventsArg {
            ts: ptr::from_ref(ts).expose_provenance() as u64,
            ..IoUringGeteventsArg::default()
        });
        let (argp, argsz) = match &arg {
            Some(a) => {
                flags |= IORING_ENTER_EXT_ARG;
                (
                    ptr::from_ref(a).cast::<c_void>(),
                    size_of::<IoUringGeteventsArg>(),
                )
            }
            None => (ptr::null(), 0),
        };
        // SAFETY: `argp` is null or points to `arg`, a 24-byte `io_uring_getevents_arg` whose
        // `ts` points to `ts`; both are locals that outlive the call.
        let r = unsafe { io_uring_enter(fd, queued, min_complete, flags | reg_flag, argp, argsz) };
        enter_outcome(r)
    }

    /// # Safety
    /// `arg` must be valid for `opcode`, as for [`io_uring_register`].
    unsafe fn register(&self, opcode: u32, arg: *const c_void, nr_args: u32) -> io::Result<u32> {
        // SAFETY: forwarded from the caller; the fd is this ring's own.
        unsafe { io_uring_register(self.fd.as_raw_fd(), opcode, arg, nr_args) }
    }

    /// Asks the kernel which opcodes it implements.
    pub(crate) fn register_probe(&self) -> io::Result<OpcodeSet> {
        let mut probe = ProbeBuf::default();
        // SAFETY: `probe` is a probe header followed by room for `PROBE_OPS` entries, as the
        // opcode expects.
        unsafe {
            self.register(
                IORING_REGISTER_PROBE,
                ptr::from_mut(&mut probe).cast(),
                PROBE_OPS as u32,
            )
        }?;
        let mut set = OpcodeSet::default();
        for op in probe.ops.iter().take(usize::from(probe.head.ops_len)) {
            if op.flags & IO_URING_OP_SUPPORTED != 0 {
                set.insert(op.op);
            }
        }
        Ok(set)
    }

    /// Registers an empty table of `slots` fixed files: an entry then names a slot
    /// (`IOSQE_FIXED_FILE`), and the table holds its own reference to the file a slot was given,
    /// so closing the descriptor it came from changes nothing for the ring.
    pub(crate) fn register_files_sparse(&self, slots: u32) -> io::Result<()> {
        let table = IoUringRsrcRegister {
            nr: slots,
            flags: IORING_RSRC_REGISTER_SPARSE,
            ..IoUringRsrcRegister::default()
        };
        // SAFETY: the opcode reads one `io_uring_rsrc_register` of the size passed.
        unsafe {
            self.register(
                IORING_REGISTER_FILES2,
                ptr::from_ref(&table).cast(),
                size_of::<IoUringRsrcRegister>() as u32,
            )
        }
        .map_err(refused("sparse registered file tables"))?;
        Ok(())
    }

    /// Puts the file of `fd` into fixed slot `slot` (`fd` -1 empties the slot). An operation
    /// already in flight on the slot keeps the file it started with.
    pub(crate) fn update_file(&self, slot: u32, fd: RawFd) -> io::Result<()> {
        let fds = [fd];
        let update = IoUringRsrcUpdate2 {
            offset: slot,
            data: fds.as_ptr().expose_provenance() as u64,
            nr: 1,
            ..IoUringRsrcUpdate2::default()
        };
        // SAFETY: the opcode reads one `io_uring_rsrc_update2` of the size passed, whose `data`
        // points to `fds`, a local that outlives the call.
        let n = unsafe {
            self.register(
                IORING_REGISTER_FILES_UPDATE2,
                ptr::from_ref(&update).cast(),
                size_of::<IoUringRsrcUpdate2>() as u32,
            )
        }?;
        if n != 1 {
            return Err(io::Error::other("the kernel did not update the fixed file"));
        }
        Ok(())
    }

    /// Sets the async worker limits `[bounded, unbounded]` (0 leaves one unchanged) and returns the
    /// limits that were in force before.
    pub(crate) fn register_iowq_max_workers(&self, limits: [u32; 2]) -> io::Result<[u32; 2]> {
        let mut limits = limits;
        // SAFETY: the opcode reads and writes two u32 values at the pointer, which `limits` holds.
        unsafe {
            self.register(
                IORING_REGISTER_IOWQ_MAX_WORKERS,
                limits.as_mut_ptr().cast(),
                2,
            )
        }?;
        Ok(limits)
    }

    fn apply_deferred_iowq(&mut self) -> io::Result<()> {
        if let Some(limits) = self.deferred_iowq.take() {
            self.register_iowq_max_workers(limits)?;
        }
        Ok(())
    }

    /// Enables a ring created with `start_disabled`; the calling thread becomes its submitter.
    pub(crate) fn enable(&mut self) -> io::Result<()> {
        // SAFETY: this opcode takes no argument.
        unsafe { self.register(IORING_REGISTER_ENABLE_RINGS, ptr::null(), 0) }?;
        self.apply_deferred_iowq()
    }

    /// Registers the ring fd with this thread so later enters skip the fd lookup. Registering twice
    /// is a no-op.
    pub(crate) fn register_ring_fd(&mut self) -> io::Result<()> {
        if self.reg_index.is_some() {
            return Ok(());
        }
        let mut update = IoUringRsrcUpdate {
            offset: u32::MAX,
            resv: 0,
            data: self.fd.as_raw_fd() as u64,
        };
        // SAFETY: the opcode reads one `io_uring_rsrc_update` and writes the chosen index back.
        let n = unsafe {
            self.register(
                IORING_REGISTER_RING_FDS,
                ptr::from_mut(&mut update).cast(),
                1,
            )
        }?;
        if n != 1 {
            return Err(io::Error::other("the kernel did not register the ring fd"));
        }
        self.reg_index = Some(update.offset);
        Ok(())
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        if let Some(index) = self.reg_index.take() {
            let update = IoUringRsrcUpdate {
                offset: index,
                ..IoUringRsrcUpdate::default()
            };
            // SAFETY: the opcode reads one `io_uring_rsrc_update` naming the slot taken above.
            // The result is dropped on purpose: a slot that cannot be released here is released
            // when the thread exits, and there is no caller left to tell.
            let _ = unsafe {
                self.register(IORING_UNREGISTER_RING_FDS, ptr::from_ref(&update).cast(), 1)
            };
        }
    }
}
