//! A child's exit, as its [`Child`](super::Child) and the shard that
//! watches it share it.
//!
//! One shard at a time watches a child: the one its waiter first polled on,
//! or a runtime worker for one whose `Child` is gone. Its driver reports
//! the exit and reaps the child there, through the child's pidfd on Linux
//! (in the poller, or a `WAITID` operation on the ring), `EVFILT_PROC` on
//! kqueue, and a wait packet on the process handle on Windows; the status
//! is stored once and the waiter woken. A shard that stops running lets its
//! children go only once their registrations ended, so no two reapers of
//! one child ever run at once. Where Linux has no pidfd (before 5.3, or a
//! filter denies it), a reaper on `SIGCHLD` takes those children instead.
//!
//! A pid stays the child's until the child is reaped, and may name another
//! process after: so no signal goes by pid once the child is reaped. On
//! Linux signals go through the pidfd, which names the process itself;
//! elsewhere a kill and a reap exclude each other through the child's state.

use core::task::{Context, Poll};
use std::io;
use std::process::ExitStatus;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::sync::atomic_waker::AtomicWaker;
use std::sync::Arc;

/// A child's exit: what is known of it, who waits for it, who watches it.
pub(crate) struct Exit {
    pid: u32,
    /// The child's pidfd: `None` where Linux gave none.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pidfd: Option<std::os::fd::OwnedFd>,
    /// The process handle, the exit's own copy.
    #[cfg(windows)]
    process: std::os::windows::io::OwnedHandle,
    /// Kill and reap by pid exclude each other (see the module).
    #[cfg(unix)]
    state: core::sync::atomic::AtomicU8,
    /// The status, or the errno of a wait that failed; set once.
    result: OnceLock<Result<ExitStatus, i32>>,
    waker: AtomicWaker,
    /// A shard watches the child, or is about to.
    watched: AtomicBool,
}

/// The child may be reaped or signalled.
#[cfg(unix)]
const ALIVE: u8 = 0;
/// A kill or a reap by pid is under way.
#[cfg(unix)]
const BUSY: u8 = 1;
/// The child was reaped.
#[cfg(unix)]
const REAPED: u8 = 2;

impl Exit {
    /// The exit of `child`, just spawned.
    pub(crate) fn new(child: &std::process::Child) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            pid: child.id(),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            pidfd: if no_pidfd() {
                None
            } else {
                pidfd_open(child.id())
            },
            #[cfg(windows)]
            process: {
                use std::os::windows::io::AsHandle;
                child.as_handle().try_clone_to_owned()?
            },
            #[cfg(unix)]
            state: core::sync::atomic::AtomicU8::new(ALIVE),
            result: OnceLock::new(),
            waker: AtomicWaker::new(),
            watched: AtomicBool::new(false),
        }))
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) fn pidfd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        self.pidfd.as_ref().map(AsRawFd::as_raw_fd)
    }

    #[cfg(windows)]
    pub(crate) fn process(&self) -> crate::syscore::windows::HANDLE {
        use std::os::windows::io::AsRawHandle;
        self.process.as_raw_handle()
    }

    /// The outcome, once known.
    fn outcome(&self) -> Option<io::Result<ExitStatus>> {
        self.result
            .get()
            .map(|r| r.map_err(io::Error::from_raw_os_error))
    }

    /// Stores the outcome (the first one stands) and wakes the waiter.
    pub(crate) fn finish(&self, result: Result<ExitStatus, i32>) {
        let _ = self.result.set(result);
        self.waker.wake();
    }

    /// Whether the outcome is known.
    pub(crate) fn finished(&self) -> bool {
        self.result.get().is_some()
    }

    /// Ready with the child's status once it exited.
    pub(crate) fn poll_wait(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<ExitStatus>> {
        if let Some(outcome) = self.outcome() {
            return Poll::Ready(outcome);
        }
        self.waker.register(cx.waker());
        self.ensure_watched();
        match self.outcome() {
            Some(outcome) => Poll::Ready(outcome),
            None => Poll::Pending,
        }
    }

    /// Has a shard (or the `SIGCHLD` reaper) watch the child, unless one
    /// does.
    pub(crate) fn ensure_watched(self: &Arc<Self>) {
        if self.finished() || self.watched.swap(true, Ordering::SeqCst) {
            return;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if self.pidfd.is_none() {
            super::fallback::watch(self);
            return;
        }
        crate::rt::shard::watch_exit(self);
    }

    /// Its watching shard let the child go, its registration ended: the
    /// waiter's next poll watches it again, wherever it runs; with no
    /// waiter (the `Child` is gone), a runtime worker watches it now.
    pub(crate) fn unwatched(self: &Arc<Self>, orphan: bool) {
        self.watched.store(false, Ordering::SeqCst);
        if orphan {
            if !crate::rt::shard::watch_exit_elsewhere(self) {
                self.watched.store(false, Ordering::SeqCst);
            }
        } else {
            self.waker.wake();
        }
    }

    /// The `Child` goes: a child still running is watched, by the shard
    /// running here or a running runtime's, so it is reaped once it exits.
    /// With no runtime running it is left to the system, as std leaves it.
    #[cfg(unix)]
    pub(crate) fn watch_orphan(self: &Arc<Self>) {
        if self.finished() || self.watched.swap(true, Ordering::SeqCst) {
            return;
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if self.pidfd.is_none() {
            super::fallback::watch(self);
            return;
        }
        let here = crate::rt::context::current(|current| {
            current.map(|shard| shard.watch(Arc::clone(self))).is_some()
        });
        if !here && !crate::rt::shard::watch_exit_elsewhere(self) {
            self.watched.store(false, Ordering::SeqCst);
        }
    }

    /// The status, if the child exited, without reaping it (the watching
    /// shard does); a shard is set to watch it, so it is reaped.
    pub(crate) fn try_wait(self: &Arc<Self>) -> io::Result<Option<ExitStatus>> {
        if let Some(outcome) = self.outcome() {
            return outcome.map(Some);
        }
        self.ensure_watched();
        let peeked = self.peek()?;
        if let Some(status) = peeked {
            // The reap that follows stores the same status.
            let _ = self.result.set(Ok(status));
        }
        Ok(peeked)
    }

    /// Ends the child (`SIGKILL`; `TerminateProcess` on Windows). A child
    /// that exited already is left alone.
    pub(crate) fn kill(&self) -> io::Result<()> {
        if self.finished() {
            return Ok(());
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(fd) = self.pidfd() {
            // SAFETY: a plain syscall on the child's own pidfd, which names
            // the process even once its pid is free.
            let r = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd,
                    libc::SIGKILL,
                    core::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            return match r {
                0 => Ok(()),
                _ => match io::Error::last_os_error() {
                    // It exited: nothing to end.
                    e if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                    e => Err(e),
                },
            };
        }
        #[cfg(unix)]
        {
            if !self.busy() {
                return Ok(());
            }
            // SAFETY: a plain syscall; the pid is the child's, which no reap
            // can free while the state says busy.
            let r = unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
            let err = io::Error::last_os_error();
            self.state.store(ALIVE, Ordering::SeqCst);
            match r {
                0 => Ok(()),
                _ if err.raw_os_error() == Some(libc::ESRCH) => Ok(()),
                _ => Err(err),
            }
        }
        #[cfg(windows)]
        {
            // SAFETY: the exit's own process handle.
            if unsafe { crate::syscore::windows::TerminateProcess(self.process(), 1) } != 0 {
                return Ok(());
            }
            match io::Error::last_os_error() {
                // It exited already.
                _ if self.peek().ok().flatten().is_some() => Ok(()),
                e => Err(e),
            }
        }
    }

    /// Takes the state from alive to busy, waiting out a kill or reap under
    /// way; false once the child is reaped.
    #[cfg(unix)]
    fn busy(&self) -> bool {
        loop {
            match self
                .state
                .compare_exchange(ALIVE, BUSY, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(REAPED) => return false,
                // One syscall away from its end.
                Err(_) => std::thread::yield_now(),
            }
        }
    }

    /// Reaps the child by pid if it exited (`waitpid`, no wait), excluding
    /// a kill: the driver of a kqueue shard, and the `SIGCHLD` reaper.
    /// `None`: still running.
    #[cfg(unix)]
    pub(crate) fn reap_by_pid(&self) -> Option<Result<ExitStatus, i32>> {
        use std::os::unix::process::ExitStatusExt;
        if !self.busy() {
            return None;
        }
        let mut raw = 0;
        // SAFETY: a plain syscall on our own child's pid; the status goes
        // to our own int.
        let r = unsafe { libc::waitpid(self.pid as libc::pid_t, &mut raw, libc::WNOHANG) };
        let errno = io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO);
        let (state, outcome) = match r {
            0 => (ALIVE, None),
            r if r > 0 => (REAPED, Some(Ok(ExitStatus::from_raw(raw)))),
            // ECHILD: something else reaped it (`SIGCHLD` ignored, or a
            // `waitpid(-1)` of the program's own): its status is lost.
            _ => (REAPED, Some(Err(errno))),
        };
        self.state.store(state, Ordering::SeqCst);
        outcome
    }

    /// Reaps the child through its pidfd if it exited: the driver of a
    /// poller or a ring without `WAITID`. `None`: still running.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) fn reap_pidfd(&self) -> Option<Result<ExitStatus, i32>> {
        let fd = self.pidfd()?;
        waitid_pidfd(fd, libc::WEXITED | libc::WNOHANG)
    }

    /// The status, if the child exited, leaving it unreaped.
    fn peek(&self) -> io::Result<Option<ExitStatus>> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(fd) = self.pidfd() {
            return match waitid_pidfd(fd, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) {
                None => Ok(None),
                Some(r) => r.map(Some).map_err(io::Error::from_raw_os_error),
            };
        }
        #[cfg(unix)]
        {
            // SAFETY: plain data; the kernel fills it.
            let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
            // SAFETY: a plain syscall on our own child, filling our own info.
            let r = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if r != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(status_of(&info))
        }
        #[cfg(windows)]
        {
            windows_status(self.process())
        }
    }
}

/// The status a `waitid` filled `info` with, `None` if no child changed
/// state (a `WNOHANG` call with nothing to report).
#[cfg(unix)]
pub(crate) fn status_of(info: &libc::siginfo_t) -> Option<ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    // SAFETY: the kernel filled the child fields of a `waitid` info.
    let (pid, code, status) = unsafe { (info.si_pid(), info.si_code, info.si_status()) };
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    let (pid, code, status) = (info.si_pid, info.si_code, info.si_status);
    if pid == 0 {
        return None;
    }
    // The wait status `waitpid` would have given.
    let raw = match code {
        libc::CLD_EXITED => (status & 0xff) << 8,
        libc::CLD_DUMPED => (status & 0x7f) | 0x80,
        _ => status & 0x7f,
    };
    Some(ExitStatus::from_raw(raw))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn waitid_pidfd(fd: std::os::fd::RawFd, options: libc::c_int) -> Option<Result<ExitStatus, i32>> {
    // SAFETY: plain data; the kernel fills it.
    let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
    // SAFETY: a plain syscall on the child's own pidfd, filling our info.
    let r = unsafe { libc::waitid(libc::P_PIDFD, fd as libc::id_t, &mut info, options) };
    if r != 0 {
        return Some(Err(io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)));
    }
    status_of(&info).map(Ok)
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
thread_local! {
    /// Children spawned on this thread get no pidfd: the `SIGCHLD` reaper
    /// takes them, as where Linux has none to give.
    pub(crate) static NO_PIDFD: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn no_pidfd() -> bool {
    #[cfg(test)]
    return NO_PIDFD.with(core::cell::Cell::get);
    #[cfg(not(test))]
    false
}

/// The child's pidfd, or `None` where the kernel has none to give (before
/// 5.3) or a filter denies it: the `SIGCHLD` reaper serves the child then.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn pidfd_open(pid: u32) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: a plain syscall; the descriptor it returns is owned at once.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: a new descriptor nothing else owns.
    Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as std::os::fd::RawFd) })
}

/// The status of a process that ended, `None` while it runs.
#[cfg(windows)]
pub(crate) fn windows_status(
    process: crate::syscore::windows::HANDLE,
) -> io::Result<Option<ExitStatus>> {
    use std::os::windows::process::ExitStatusExt;

    use crate::syscore::windows::{GetExitCodeProcess, WAIT_OBJECT_0, WaitForSingleObject};
    // SAFETY: a plain call on a live process handle.
    if unsafe { WaitForSingleObject(process, 0) } != WAIT_OBJECT_0 {
        return Ok(None);
    }
    let mut code = 0u32;
    // SAFETY: as above; the code goes to our own u32.
    if unsafe { GetExitCodeProcess(process, &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(ExitStatus::from_raw(code)))
}
