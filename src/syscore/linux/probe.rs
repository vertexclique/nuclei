//! Whether io_uring can serve this process, decided without risking the process.
//!
//! [`probe`] answers one question for the backend choice: can a [`Ring`] be created here? The
//! answer is [`Probe::Available`] or [`Probe::Unavailable`] with the reason, never a panic and
//! never a hang.
//!
//! The danger is a seccomp filter whose default action kills the caller (some systemd units and
//! sandboxes do this for `io_uring_setup`): calling the syscall in-process would end the whole
//! program, not just fail. So the probe first asks `prctl(PR_GET_SECCOMP)`:
//!
//! * `0` (no filter) means nothing can kill the process on the call, so a real [`Ring`] is built
//!   and dropped right there and its error, if any, becomes the reason.
//! * anything else (a filter is active, the question itself failed, a mode this code does not
//!   know) sends the one `io_uring_setup` call into a short-lived child made with
//!   `clone(CLONE_VM | CLONE_VFORK | SIGCHLD)` on a stack mapped for it. The child makes only raw
//!   syscalls (no allocation, no locks), so what a filter does to it can only end the child. The
//!   parent is suspended until the child is gone, which also means no second thread of the child
//!   exists to race with. Signals are deliberately left alone: a signal that is blocked when the
//!   child exits would not be discarded even when its disposition ignores it (`SIGCHLD` by
//!   default), and would interrupt a blocking syscall of some unrelated thread of the program.
//! * A kill filter on the last thread of a process makes the kernel write a core dump, and the
//!   child shares the whole address space of the program, so the dump would be a copy of it (and
//!   a crash report under a core handler such as systemd-coredump) on every start. So the
//!   child's first call lowers its own soft core limit to 1, which makes the kernel skip both
//!   piped and file dumps; limits belong to the process, so the parent keeps its own. The call is
//!   best effort: if it fails, a dump may still be written. The cost is a filter that allows
//!   `io_uring_setup` but kills `prlimit64`: the child then dies before the probe call, the
//!   outcome is [`Unavailable::KilledBySeccomp`], and the runtime falls back to epoll.
//!
//! The child reports through a pipe, not through its exit status: a byte written to the pipe means
//! the syscall was allowed, and only then does the parent build the real [`Ring`] in-process (the
//! child proves the call is not killed, the ring proves everything else). No byte means the exit
//! status says why: the errno on a refusal, or death by `SIGSYS` for a kill filter. The pipe is
//! what survives when the status is unavailable, as when the program set `SIGCHLD` to ignore and
//! the kernel reaped the child on its own.
//!
//! Nothing under `/proc` is read by the probe: the seccomp mode comes from `prctl`, which works
//! where `/proc` is missing or masked.

use std::{
    ffi::c_void,
    fmt,
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    ptr,
};

use super::iouring::ring::{IoUringParams, Refusal, Ring, RingConfig, setup_params};

/// Submission entries of the probe ring and of the child's trial ring: the smallest useful size.
const PROBE_ENTRIES: u32 = 4;
/// The child needs a few hundred bytes of stack; 64 KiB leaves ample room for the libc wrappers.
const CHILD_STACK: usize = 64 * 1024;

/// What the probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    /// A ring was created here; io_uring can be used.
    Available,
    /// io_uring cannot be used here, for the reason inside.
    Unavailable(Unavailable),
}

/// Why io_uring cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// The kernel has no io_uring (`ENOSYS`).
    NotInKernel,
    /// The system refused the call (`EPERM` or `EACCES`, the code inside).
    Denied(i32),
    /// A seccomp filter kills any process that makes the call.
    KilledBySeccomp,
    /// The kernel refused the flags every ring needs, or lacks a feature every ring needs.
    MissingFeatures,
    /// The system ran out of a resource (`ENOMEM`, `EMFILE` or `ENFILE`, the code inside).
    Resources(i32),
    /// The check itself could not run or reported nothing usable; the reason inside.
    ProbeFailed(String),
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInKernel => f.write_str("this kernel has no io_uring support"),
            Self::Denied(code) => write!(
                f,
                "the system refused io_uring: {}",
                io::Error::from_raw_os_error(*code)
            ),
            Self::KilledBySeccomp => {
                f.write_str("a seccomp filter ends any process that calls io_uring")
            }
            Self::MissingFeatures => {
                f.write_str("the kernel lacks the io_uring features this runtime needs")
            }
            Self::Resources(code) => write!(
                f,
                "the system has no room for an io_uring ring: {}",
                io::Error::from_raw_os_error(*code)
            ),
            Self::ProbeFailed(why) => write!(f, "the io_uring check could not run: {why}"),
        }
    }
}

impl std::error::Error for Unavailable {}

/// Which path produced a [`Probe`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    InProcess,
    Child,
}

/// The seccomp mode of the calling thread (`0` off, `1` strict, `2` filter), or `None` when the
/// kernel cannot say.
pub(crate) fn seccomp_mode() -> Option<u32> {
    // SAFETY: PR_GET_SECCOMP takes no arguments and only reads state of the calling thread.
    let r = unsafe { libc::prctl(libc::PR_GET_SECCOMP) };
    u32::try_from(r).ok()
}

/// Checks whether io_uring can serve this process. See the module documentation for how it stays
/// safe under a seccomp filter.
pub(crate) fn probe() -> Probe {
    probe_with(false).0
}

/// [`probe`], optionally forcing the child path, and which path ran.
fn probe_with(force_child: bool) -> (Probe, Path) {
    if !force_child && seccomp_mode() == Some(0) {
        return (ring_probe(), Path::InProcess);
    }
    let outcome = match child_probe() {
        Ok(()) => ring_probe(),
        Err(why) => Probe::Unavailable(why),
    };
    (outcome, Path::Child)
}

/// Builds and drops a small real ring.
fn ring_probe() -> Probe {
    match Ring::new(&RingConfig::new(PROBE_ENTRIES)) {
        Ok(_) => Probe::Available,
        Err(e) => Probe::Unavailable(classify(&e)),
    }
}

/// The one mapping from a failed ring creation, or a child's errno, to a reason.
fn classify(e: &io::Error) -> Unavailable {
    if let Some(refusal) = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Refusal>())
    {
        return match refusal {
            Refusal::NotInKernel => Unavailable::NotInKernel,
            Refusal::FloorFlags | Refusal::Feature(_) => Unavailable::MissingFeatures,
        };
    }
    match e.raw_os_error() {
        Some(libc::ENOSYS) => Unavailable::NotInKernel,
        Some(code @ (libc::EPERM | libc::EACCES)) => Unavailable::Denied(code),
        Some(libc::EINVAL) => Unavailable::MissingFeatures,
        Some(code @ (libc::ENOMEM | libc::EMFILE | libc::ENFILE)) => Unavailable::Resources(code),
        _ => Unavailable::ProbeFailed(e.to_string()),
    }
}

/// Everything the parent learned from one child.
struct ChildReport {
    pid: libc::pid_t,
    /// The child wrote its byte: the syscall was allowed.
    wrote: bool,
    /// The child's wait status, or why it could not be collected.
    status: io::Result<libc::c_int>,
}

/// Runs the trial `io_uring_setup` in a child. `Ok` means the call is allowed (a ring built next
/// is the caller's job); `Err` is the reason the child gave, or why there is none.
fn child_probe() -> Result<(), Unavailable> {
    interpret(&run_child()?)
}

/// What a finished child's report means. Pure, so every wait status is testable.
fn interpret(report: &ChildReport) -> Result<(), Unavailable> {
    if report.wrote {
        return Ok(());
    }
    let status = match &report.status {
        Ok(status) => *status,
        Err(e) => {
            return Err(Unavailable::ProbeFailed(format!(
                "the probe child reported nothing and could not be waited for: {e}"
            )));
        }
    };
    if libc::WIFEXITED(status) {
        return Err(match libc::WEXITSTATUS(status) {
            0 => Unavailable::ProbeFailed("the probe child ended without a result".into()),
            errno => classify(&io::Error::from_raw_os_error(errno)),
        });
    }
    if libc::WIFSIGNALED(status) {
        return Err(match libc::WTERMSIG(status) {
            libc::SIGSYS => Unavailable::KilledBySeccomp,
            signal => {
                Unavailable::ProbeFailed(format!("the probe child was ended by signal {signal}"))
            }
        });
    }
    Err(Unavailable::ProbeFailed(format!(
        "the probe child ended in an unexpected state (status {status:#x})"
    )))
}

fn failed(what: &str, e: io::Error) -> Unavailable {
    Unavailable::ProbeFailed(format!("could not {what}: {e}"))
}

/// Creates the child, waits for it, and collects its byte and its status.
fn run_child() -> Result<ChildReport, Unavailable> {
    let stack = Stack::new().map_err(|e| failed("map a stack for the probe child", e))?;
    let (read_end, write_end) = pipe().map_err(|e| failed("create the probe pipe", e))?;
    let mut args = ChildArgs {
        write_fd: write_end.as_raw_fd(),
        core_limit: quiet_core_limit(),
        // The floor flags without the index-array option: a kernel older than 6.6 would refuse
        // that one, and the real ring negotiates it itself.
        params: setup_params(&RingConfig::new(PROBE_ENTRIES), false),
    };
    let pid = spawn(&mut args, &stack).map_err(|e| failed("start the probe child", e))?;
    // The parent was suspended until the child ended, so the write end is the only one left open
    // here and the read below sees either the byte or the end of the stream.
    drop(write_end);
    let wrote = File::from(read_end).read_exact(&mut [0u8]).is_ok();
    Ok(ChildReport {
        pid,
        wrote,
        status: reap(pid),
    })
}

/// Starts the child, retrying once when the system is momentarily out of processes (`EAGAIN`).
fn spawn(args: &mut ChildArgs, stack: &Stack) -> io::Result<libc::pid_t> {
    let flags = libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD;
    let mut attempt = 0;
    loop {
        // SAFETY: `child_main` only makes raw syscalls and `_exit`s; `args` outlives the child
        // because CLONE_VFORK suspends this thread until the child is gone; the stack is a live
        // writable mapping and `top` is its high end, as the stack grows down.
        let pid = unsafe {
            libc::clone(
                child_main,
                stack.top(),
                flags,
                ptr::from_mut(args).cast::<c_void>(),
            )
        };
        if pid >= 0 {
            return Ok(pid);
        }
        let e = io::Error::last_os_error();
        attempt += 1;
        if attempt > 1 || e.raw_os_error() != Some(libc::EAGAIN) {
            return Err(e);
        }
    }
}

/// Waits for `pid`. `ECHILD` is returned as an error like any other: the caller relies on the
/// pipe then, which is the case when the program ignores `SIGCHLD` and the kernel reaped already.
fn reap(pid: libc::pid_t) -> io::Result<libc::c_int> {
    let mut status = 0;
    loop {
        // SAFETY: `status` is a live local the kernel writes one int into.
        let r = unsafe { libc::waitpid(pid, &raw mut status, libc::__WALL) };
        if r == pid {
            return Ok(status);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// A close-on-exec, non-blocking pipe as `(read end, write end)`.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [RawFd; 2] = [-1; 2];
    // SAFETY: `fds` has room for the two descriptors the call writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `pipe2` returned two new descriptors that nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The core limit the child sets on itself: soft limit 1 (the kernel skips a dump at exactly 1,
/// piped or not) and the hard limit as it is now, so nothing is ever raised. Read here because
/// the child may do no more than raw syscalls.
fn quiet_core_limit() -> libc::rlimit {
    let mut now = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: reads one `rlimit` value into a live local.
    let max = if unsafe { libc::getrlimit(libc::RLIMIT_CORE, &raw mut now) } == 0 {
        now.rlim_max
    } else {
        1
    };
    libc::rlimit {
        rlim_cur: max.min(1),
        rlim_max: max,
    }
}

/// What the child needs, shared with it through the common address space.
#[repr(C)]
struct ChildArgs {
    write_fd: RawFd,
    core_limit: libc::rlimit,
    params: IoUringParams,
}

/// The child: a quiet core limit, one `io_uring_setup`, one byte on success, then gone. Raw
/// syscalls only: it shares the parent's memory and thread state, so it must not allocate, lock,
/// or call back into std.
extern "C" fn child_main(arg: *mut c_void) -> libc::c_int {
    // SAFETY: `arg` is the `ChildArgs` the parent keeps alive and untouched until this process
    // ends (CLONE_VFORK).
    let args = unsafe { &mut *arg.cast::<ChildArgs>() };
    // SAFETY: a raw `prlimit64` on this process (pid 0) that reads one `rlimit` and returns the
    // old limit nowhere. Its failure only means a dump may still be written, so it is ignored.
    unsafe {
        libc::syscall(
            libc::SYS_prlimit64,
            0 as libc::c_long,
            libc::RLIMIT_CORE as libc::c_long,
            ptr::from_ref(&args.core_limit),
            ptr::null_mut::<libc::rlimit>(),
        )
    };
    // SAFETY: a raw `io_uring_setup` with a valid, exclusively borrowed `io_uring_params`.
    let r = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            PROBE_ENTRIES as libc::c_long,
            ptr::from_mut(&mut args.params),
        )
    };
    if r < 0 {
        // SAFETY: the libc errno slot of this (shared) thread is valid to read.
        #[cfg(not(target_os = "android"))]
        let errno = unsafe { *libc::__errno_location() };
        // SAFETY: as above; Bionic names the slot `__errno`.
        #[cfg(target_os = "android")]
        let errno = unsafe { *libc::__errno() };
        // SAFETY: `_exit` runs no handlers and returns nothing; the ring fd, if any, closes with
        // the child's own copy of the descriptor table.
        unsafe { libc::_exit(errno.clamp(1, 255)) }
    }
    let byte = 1u8;
    // SAFETY: writes one byte from a live local to the pipe the parent left open. A failed write
    // is left for the parent to see as an empty pipe.
    unsafe { libc::write(args.write_fd, ptr::from_ref(&byte).cast(), 1) };
    // SAFETY: as above.
    unsafe { libc::_exit(0) }
}

/// An anonymous mapping that serves as the child's stack and is unmapped on drop.
struct Stack(*mut c_void);

impl Stack {
    fn new() -> io::Result<Self> {
        // SAFETY: an anonymous private mapping at a kernel-chosen address aliases nothing.
        let p = unsafe {
            libc::mmap(
                ptr::null_mut(),
                CHILD_STACK,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(p))
    }

    /// The high end of the mapping, where a downward-growing stack starts.
    fn top(&self) -> *mut c_void {
        self.0.cast::<u8>().wrapping_add(CHILD_STACK).cast()
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the mapping `new` made; the child that used it is gone.
        unsafe { libc::munmap(self.0, CHILD_STACK) };
    }
}

// The tests install seccomp filters with Linux's classic BPF constants.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod tests {
    use std::{env, fs, thread};

    use super::*;

    /// Whether this machine is declared to have a working io_uring (`NUCLEI_REQUIRE_IO_URING=1`).
    fn required() -> bool {
        env::var("NUCLEI_REQUIRE_IO_URING").as_deref() == Ok("1")
    }

    /// `true` when a ring was found; otherwise panics if io_uring is required, else says it skips.
    fn found_or_skip(probe: &Probe) -> bool {
        if *probe == Probe::Available {
            return true;
        }
        assert!(
            !required(),
            "io_uring is required but unavailable: {probe:?}"
        );
        eprintln!("skipping: io_uring unavailable: {probe:?}");
        false
    }

    #[test]
    fn in_process_path_is_taken_without_a_filter() {
        let (probe, path) = probe_with(false);
        let expected = if seccomp_mode() == Some(0) {
            Path::InProcess
        } else {
            Path::Child
        };
        assert_eq!(path, expected);
        found_or_skip(&probe);
    }

    #[test]
    fn child_path_finds_a_ring_where_the_process_does() {
        let (probe, path) = probe_with(true);
        assert_eq!(path, Path::Child);
        if probe_with(false).0 == Probe::Available {
            assert_eq!(probe, Probe::Available);
        }
        found_or_skip(&probe);
    }

    #[test]
    fn errors_map_to_reasons() {
        use Unavailable::*;
        let errno = io::Error::from_raw_os_error;
        let table = [
            (errno(libc::ENOSYS), NotInKernel),
            (errno(libc::EPERM), Denied(libc::EPERM)),
            (errno(libc::EACCES), Denied(libc::EACCES)),
            (errno(libc::EINVAL), MissingFeatures),
            (errno(libc::ENOMEM), Resources(libc::ENOMEM)),
            (errno(libc::EMFILE), Resources(libc::EMFILE)),
            (errno(libc::ENFILE), Resources(libc::ENFILE)),
            (Refusal::NotInKernel.into(), NotInKernel),
            (Refusal::FloorFlags.into(), MissingFeatures),
            (Refusal::Feature("timed waits").into(), MissingFeatures),
            (io::Error::other("odd"), ProbeFailed("odd".into())),
        ];
        for (error, want) in table {
            assert_eq!(classify(&error), want, "{error:?}");
        }
        assert!(matches!(
            classify(&errno(libc::EBUSY)),
            ProbeFailed(why) if why.contains("busy")
        ));
    }

    #[test]
    fn reasons_read_in_plain_words() {
        use Unavailable::*;
        for reason in [
            NotInKernel,
            Denied(libc::EPERM),
            KilledBySeccomp,
            MissingFeatures,
            Resources(libc::ENOMEM),
            ProbeFailed("no stack".into()),
        ] {
            let text = reason.to_string();
            assert!(
                text.contains(' ') && !text.contains("Unavailable"),
                "{text}"
            );
        }
        assert!(Denied(libc::EPERM).to_string().contains("not permitted"));
    }

    fn report(wrote: bool, status: io::Result<libc::c_int>) -> ChildReport {
        ChildReport {
            pid: 0,
            wrote,
            status,
        }
    }

    #[test]
    fn child_reports_map_to_reasons() {
        use Unavailable::*;
        let exited = |code: i32| Ok(code << 8);
        let signaled = |sig: i32| Ok(sig);
        let table = [
            (report(true, exited(0)), Ok(())),
            (
                report(true, Err(io::Error::from_raw_os_error(libc::ECHILD))),
                Ok(()),
            ),
            (report(false, exited(libc::ENOSYS)), Err(NotInKernel)),
            (report(false, exited(libc::EPERM)), Err(Denied(libc::EPERM))),
            (
                report(false, exited(libc::ENOMEM)),
                Err(Resources(libc::ENOMEM)),
            ),
            (report(false, exited(libc::EINVAL)), Err(MissingFeatures)),
            (report(false, signaled(libc::SIGSYS)), Err(KilledBySeccomp)),
            // The core-dump bit rides along when the kernel dumped the dying child.
            (
                report(false, signaled(libc::SIGSYS | 0x80)),
                Err(KilledBySeccomp),
            ),
        ];
        for (report, want) in table {
            assert_eq!(interpret(&report), want);
        }
        for odd in [
            report(false, exited(0)),
            report(false, signaled(libc::SIGKILL)),
            report(false, Ok(0x137f)),
            report(false, Err(io::Error::from_raw_os_error(libc::ECHILD))),
        ] {
            assert!(
                matches!(interpret(&odd), Err(ProbeFailed(_))),
                "{:?}",
                interpret(&odd)
            );
        }
    }

    #[test]
    fn child_path_closes_what_it_opens() {
        let open_fds = || fs::read_dir("/proc/self/fd").map(Iterator::count);
        let Ok(before) = open_fds() else {
            eprintln!("skipping: /proc is not available");
            return;
        };
        // Other tests open and close descriptors at the same time, so the bound is loose: a leak
        // of even one descriptor per run adds `RUNS`, far above what concurrent tests hold.
        const RUNS: usize = 200;
        for _ in 0..RUNS {
            run_child().expect("the probe child runs");
        }
        let after = open_fds().expect("the descriptor list reads again");
        assert!(
            after < before + RUNS / 2,
            "fds went from {before} to {after}"
        );
    }

    #[test]
    fn child_is_reaped() {
        let report = run_child().expect("the probe child runs");
        // SAFETY: signal 0 only checks that the process exists.
        let rc = unsafe { libc::kill(report.pid, 0) };
        let errno = io::Error::last_os_error().raw_os_error();
        assert!(rc != 0, "child {} is still there as a zombie", report.pid);
        assert_eq!(errno, Some(libc::ESRCH));
        let status = report.status.expect("the child was collected");
        if report.wrote {
            assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        }
    }

    #[test]
    fn seccomp_mode_matches_the_kernel_report() {
        let Ok(text) = fs::read_to_string("/proc/thread-self/status") else {
            eprintln!("skipping: /proc is not available");
            return;
        };
        let want = text
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp:"))
            .and_then(|v| v.trim().parse::<u32>().ok());
        assert_eq!(seccomp_mode(), want);
    }

    #[test]
    fn child_core_limit_never_raises_what_the_process_has() {
        let mut now = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: reads one `rlimit` value into a live local.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_CORE, &raw mut now) },
            0
        );
        let quiet = quiet_core_limit();
        assert_eq!(quiet.rlim_max, now.rlim_max);
        assert_eq!(quiet.rlim_cur, now.rlim_max.min(1));
    }

    const LD: u32 = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
    const JEQ: u32 = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
    const RET: u32 = libc::BPF_RET | libc::BPF_K;
    /// Offset of the low word of a syscall's third argument in `seccomp_data`.
    const ARG2_LOW: u32 = if cfg!(target_endian = "little") {
        32
    } else {
        36
    };

    fn insn(code: u32, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
        libc::sock_filter {
            code: code as u16,
            jt,
            jf,
            k,
        }
    }

    /// A filter program that answers syscall `nr` with `action` and allows everything else.
    pub(crate) fn answer(nr: libc::c_long, action: u32) -> Vec<libc::sock_filter> {
        vec![
            insn(LD, 0, 0, 0),
            insn(JEQ, 0, 1, nr as u32),
            insn(RET, 0, 0, action),
            insn(RET, 0, 0, libc::SECCOMP_RET_ALLOW),
        ]
    }

    /// A filter program that kills a `prlimit64` which sets a limit (new-limit pointer not null)
    /// and allows everything else, reads of a limit included.
    fn kill_limit_changes() -> Vec<libc::sock_filter> {
        vec![
            insn(LD, 0, 0, 0),
            insn(JEQ, 0, 3, libc::SYS_prlimit64 as u32),
            insn(LD, 0, 0, ARG2_LOW),
            insn(JEQ, 1, 0, 0),
            insn(RET, 0, 0, libc::SECCOMP_RET_KILL_THREAD),
            insn(RET, 0, 0, libc::SECCOMP_RET_ALLOW),
        ]
    }

    /// Installs `program` on the calling thread (and so on its children).
    pub(crate) fn install(mut program: Vec<libc::sock_filter>) -> io::Result<()> {
        let filter = libc::sock_fprog {
            len: program.len() as u16,
            filter: program.as_mut_ptr(),
        };
        // SAFETY: no-new-privs takes plain integer arguments and only restricts this thread.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `filter` points at `program`, a valid classic BPF program that outlives the call.
        let r = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                ptr::from_ref(&filter),
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// What the probe finds on a thread of its own that carries `program` (the filter is
    /// irreversible), or `None` when this machine does not let the test install one.
    fn probe_under(program: Vec<libc::sock_filter>) -> Option<(Probe, Path, Option<u32>)> {
        let outcome = thread::spawn(move || {
            install(program)?;
            let (probe, path) = probe_with(false);
            Ok::<_, io::Error>((probe, path, seccomp_mode()))
        })
        .join()
        .expect("the filtered thread does not panic");
        outcome
            .map_err(|e| eprintln!("skipping: cannot install a seccomp filter here: {e}"))
            .ok()
    }

    #[test]
    fn real_filters_are_reported_with_their_reason() {
        use Unavailable::*;
        let setup = libc::SYS_io_uring_setup;
        let errno = |code: i32| libc::SECCOMP_RET_ERRNO | code as u32;
        let table = [
            (
                answer(setup, libc::SECCOMP_RET_KILL_THREAD),
                KilledBySeccomp,
            ),
            (answer(setup, errno(libc::EPERM)), Denied(libc::EPERM)),
            (answer(setup, errno(libc::EACCES)), Denied(libc::EACCES)),
            (answer(setup, errno(libc::ENOSYS)), NotInKernel),
            (answer(setup, errno(libc::EINVAL)), MissingFeatures),
            (answer(setup, errno(libc::ENOMEM)), Resources(libc::ENOMEM)),
        ];
        for (program, want) in table {
            let Some((probe, path, mode)) = probe_under(program) else {
                return;
            };
            assert_eq!(mode, Some(2));
            assert_eq!(
                path,
                Path::Child,
                "the filtered call must not run in-process"
            );
            assert_eq!(probe, Probe::Unavailable(want));
        }
    }

    /// The child lowers its core limit before anything else, so a filter that kills that call
    /// ends it ahead of the probe call: the price of the quiet exit, written in the module docs.
    /// It also shows the child really makes the call. That kill is the one case where a dump is
    /// written, so the test process lowers its own soft limit meanwhile and puts it back.
    #[test]
    fn a_filter_that_kills_the_core_limit_call_ends_the_child_first() {
        let get = |limit: &mut libc::rlimit| {
            // SAFETY: reads one `rlimit` value into a live local.
            assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, limit) }, 0);
        };
        let set = |limit: &libc::rlimit| {
            // SAFETY: writes one `rlimit` value held by a live local.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, limit) }, 0);
        };
        let mut saved = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        get(&mut saved);
        set(&quiet_core_limit());
        let found = probe_under(kill_limit_changes());
        set(&saved);
        if let Some((probe, path, _)) = found {
            assert_eq!(path, Path::Child);
            assert_eq!(probe, Probe::Unavailable(Unavailable::KilledBySeccomp));
        }
    }

    fn errno_name(code: i32) -> String {
        match code {
            libc::EPERM => "EPERM".into(),
            libc::EACCES => "EACCES".into(),
            libc::ENOMEM => "ENOMEM".into(),
            libc::EMFILE => "EMFILE".into(),
            libc::ENFILE => "ENFILE".into(),
            other => other.to_string(),
        }
    }

    fn describe(probe: &Probe) -> String {
        match probe {
            Probe::Available => "Available".into(),
            Probe::Unavailable(Unavailable::Denied(c)) => {
                format!("Unavailable(Denied({}))", errno_name(*c))
            }
            Probe::Unavailable(Unavailable::Resources(c)) => {
                format!("Unavailable(Resources({}))", errno_name(*c))
            }
            Probe::Unavailable(other) => format!("Unavailable({other:?})"),
        }
    }

    /// One line for the sandbox matrix (`benches/reference/probe-matrix.sh`); always exits 0, the
    /// script judges the outcome.
    #[test]
    #[ignore = "prints one line for the sandbox matrix"]
    fn probe_report() {
        let mode = seccomp_mode().map_or_else(|| "unknown".into(), |m| m.to_string());
        let (probe, path) = probe_with(false);
        let path = match path {
            Path::InProcess => "in-process",
            Path::Child => "child",
        };
        println!(
            "probe seccomp={mode} outcome={} path={path}",
            describe(&probe)
        );
    }
}
