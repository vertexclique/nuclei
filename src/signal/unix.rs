//! Unix signals as streams: [`signal`] listens to one [`SignalKind`].
//!
//! The first listener of a signal installs the process's handler for it
//! (`SA_SIGINFO | SA_RESTART | SA_ONSTACK`), which stays for good and
//! chains to the handler that was there before. The handler raises the
//! signal's count, then writes to the process's wake descriptor: an
//! eventfd on Linux, a pipe elsewhere, made once, non-blocking, and never
//! closed, so a handler never writes to a descriptor that went or that
//! another file reuses. Both are async-signal-safe; errno is kept.
//!
//! Each shard a listener is polled on runs a pump: a task of the runtime's
//! own that reads the wake descriptor through the shard's driver and then
//! dispatches. The pump drains the descriptor before it looks at the
//! counts, and the handler raises the count before it writes, so a
//! delivery whose write the pump read is in the counts it sees, and a later
//! one writes again. A listener polled where no shard runs is served by a
//! pump on a runtime worker.

use core::cell::{Cell, UnsafeCell};
use core::ffi::{c_int, c_void};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::os::fd::{BorrowedFd, RawFd};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};

use super::{Listener, SLOTS};
use crate::io::Handle;

/// A unix signal a [`Signal`] listens to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SignalKind(c_int);

impl SignalKind {
    /// The signal numbered `signum`.
    pub const fn from_raw(signum: c_int) -> Self {
        Self(signum)
    }

    /// The signal's number.
    pub const fn as_raw(self) -> c_int {
        self.0
    }

    /// `SIGALRM`.
    pub const fn alarm() -> Self {
        Self(libc::SIGALRM)
    }

    /// `SIGCHLD`.
    pub const fn child() -> Self {
        Self(libc::SIGCHLD)
    }

    /// `SIGHUP`.
    pub const fn hangup() -> Self {
        Self(libc::SIGHUP)
    }

    /// `SIGINT`: Ctrl-C at a terminal.
    pub const fn interrupt() -> Self {
        Self(libc::SIGINT)
    }

    /// `SIGPIPE`.
    pub const fn pipe() -> Self {
        Self(libc::SIGPIPE)
    }

    /// `SIGQUIT`.
    pub const fn quit() -> Self {
        Self(libc::SIGQUIT)
    }

    /// `SIGTERM`.
    pub const fn terminate() -> Self {
        Self(libc::SIGTERM)
    }

    /// `SIGUSR1`.
    pub const fn user_defined1() -> Self {
        Self(libc::SIGUSR1)
    }

    /// `SIGUSR2`.
    pub const fn user_defined2() -> Self {
        Self(libc::SIGUSR2)
    }

    /// `SIGWINCH`: the terminal's size changed.
    pub const fn window_change() -> Self {
        Self(libc::SIGWINCH)
    }
}

impl From<c_int> for SignalKind {
    fn from(signum: c_int) -> Self {
        Self(signum)
    }
}

/// Signals no handler can take, or whose handler cannot return.
const FORBIDDEN: [c_int; 5] = [
    libc::SIGILL,
    libc::SIGFPE,
    libc::SIGKILL,
    libc::SIGSEGV,
    libc::SIGSTOP,
];

/// The deliveries of one signal, as a stream: each item stands for one
/// delivery or more since the item before.
#[derive(Debug)]
pub struct Signal {
    listener: Listener,
}

/// Listens to `kind` from now on.
///
/// # Errors
///
/// When `kind` is no signal number, is one no handler can take (`SIGKILL`,
/// `SIGSTOP`) or whose handler cannot return (`SIGILL`, `SIGFPE`,
/// `SIGSEGV`), or when the handler or the wake descriptor cannot be made.
pub fn signal(kind: SignalKind) -> io::Result<Signal> {
    let signum = kind.0;
    if signum <= 0 || signum as usize >= SLOTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a signal number",
        ));
    }
    if FORBIDDEN.contains(&signum) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "this signal cannot be listened to",
        ));
    }
    install(signum)?;
    Ok(Signal {
        listener: Listener::new(signum as usize),
    })
}

impl Signal {
    /// Waits for the next delivery. Always `Some`: the stream never ends.
    pub async fn recv(&mut self) -> Option<()> {
        core::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Ready with `Some` once the signal was delivered since the last item.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<()>> {
        self.listener.poll_recv(cx, ensure_pump)
    }
}

impl futures_core::Stream for Signal {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<()>> {
        self.get_mut().poll_recv(cx)
    }
}

/// The wake descriptor: the end the handler writes and the end the pumps
/// read (the same one for an eventfd), and how many bytes a write is.
struct Wake {
    write: RawFd,
    read: RawFd,
    len: usize,
}

/// The write end for the handler: -1 until the wake descriptor is made.
static WAKE_WRITE: AtomicI32 = AtomicI32::new(-1);
/// What a write is, in bytes, for the handler.
static WAKE_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1);

/// The process's wake descriptor, made on first need. It is never closed:
/// the handler may write to it at any time, and the pumps read it.
fn wake() -> io::Result<&'static Wake> {
    static WAKE: OnceLock<Result<Wake, i32>> = OnceLock::new();
    let made = WAKE.get_or_init(|| {
        let wake = make_wake().map_err(|err| err.raw_os_error().unwrap_or(libc::EIO))?;
        WAKE_LEN.store(wake.len, Ordering::SeqCst);
        WAKE_WRITE.store(wake.write, Ordering::SeqCst);
        Ok(wake)
    });
    made.as_ref()
        .map_err(|&errno| io::Error::from_raw_os_error(errno))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn make_wake() -> io::Result<Wake> {
    // SAFETY: a plain syscall; the descriptor is the process's for good.
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Wake {
        write: fd,
        read: fd,
        len: size_of::<u64>(),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn make_wake() -> io::Result<Wake> {
    let mut fds = [-1; 2];
    // SAFETY: the kernel writes the two descriptors into our array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: flag changes on the descriptors just made.
        let set = unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0
                && libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) == 0
        };
        if !set {
            let err = io::Error::last_os_error();
            // SAFETY: closes the two descriptors just made, which nothing
            // else has.
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(err);
        }
    }
    Ok(Wake {
        write: fds[1],
        read: fds[0],
        len: 1,
    })
}

const IDLE: u8 = 0;
const INSTALLING: u8 = 1;
const INSTALLED: u8 = 2;

/// The process's handler for one signal.
struct Handler {
    state: AtomicU8,
    /// The action that was in place before; written before the handler
    /// is installed, and then only read.
    previous: UnsafeCell<MaybeUninit<libc::sigaction>>,
    /// `previous` is written.
    chains: AtomicBool,
}

// SAFETY: `previous` is written once, before `chains` is raised and before
// the handler that reads it is installed, and only read after.
unsafe impl Sync for Handler {}

static HANDLERS: [Handler; SLOTS] = [const {
    Handler {
        state: AtomicU8::new(IDLE),
        previous: UnsafeCell::new(MaybeUninit::uninit()),
        chains: AtomicBool::new(false),
    }
}; SLOTS];

/// Installs the handler of `signum` (a number below `SLOTS`), once.
fn install(signum: c_int) -> io::Result<()> {
    let handler = &HANDLERS[signum as usize];
    loop {
        match handler
            .state
            .compare_exchange(IDLE, INSTALLING, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => break,
            Err(INSTALLED) => return Ok(()),
            // Another thread installs it: its outcome is a moment away.
            Err(_) => std::thread::yield_now(),
        }
    }
    let installed = wake().and_then(|_| {
        // SAFETY: plain data, all zeroes valid; the kernel fills it.
        let mut previous: libc::sigaction = unsafe { core::mem::zeroed() };
        // SAFETY: reads the action in place into our own struct.
        if unsafe { libc::sigaction(signum, core::ptr::null(), &mut previous) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if !handler.chains.load(Ordering::Acquire) {
            // SAFETY: the one writer (the installing thread), before the
            // handler that reads it is in place.
            unsafe { (*handler.previous.get()).write(previous) };
            handler.chains.store(true, Ordering::Release);
        }
        // SAFETY: as above.
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
        action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART | libc::SA_ONSTACK;
        // SAFETY: an empty mask in our own struct; then the action is ours,
        // its function lives for the process.
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signum, &action, core::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    });
    let state = if installed.is_ok() { INSTALLED } else { IDLE };
    handler.state.store(state, Ordering::Release);
    installed
}

/// The process's handler: counts the delivery, wakes the pumps, and runs
/// the handler that was there before. Async-signal-safe: atomics, one
/// `write`, and errno as it found it.
extern "C" fn on_signal(signum: c_int, info: *mut libc::siginfo_t, context: *mut c_void) {
    // SAFETY: the thread's own errno.
    let errno = unsafe { *errno_location() };
    super::delivered(signum as usize);
    let fd = WAKE_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        let one = 1u64;
        // SAFETY: writes the first bytes of our own u64 to a descriptor
        // that is never closed; a full pipe (EAGAIN) has a wake waiting
        // already.
        unsafe {
            libc::write(
                fd,
                (&one as *const u64).cast(),
                WAKE_LEN.load(Ordering::SeqCst),
            )
        };
    }
    if let Some(handler) = HANDLERS.get(signum as usize)
        && handler.chains.load(Ordering::Acquire)
    {
        // SAFETY: written before `chains` was raised, read-only since.
        let previous = unsafe { (*handler.previous.get()).assume_init_ref() };
        let action = previous.sa_sigaction;
        if action != libc::SIG_DFL && action != libc::SIG_IGN {
            if previous.sa_flags & libc::SA_SIGINFO != 0 {
                // SAFETY: a handler installed with SA_SIGINFO takes these.
                let chained: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                    unsafe { core::mem::transmute(action) };
                chained(signum, info, context);
            } else {
                // SAFETY: a plain handler takes the signal number.
                let chained: extern "C" fn(c_int) = unsafe { core::mem::transmute(action) };
                chained(signum);
            }
        }
    }
    // SAFETY: as above.
    unsafe { *errno_location() = errno };
}

#[cfg(target_os = "linux")]
fn errno_location() -> *mut c_int {
    // SAFETY: the thread's errno; no argument.
    unsafe { libc::__errno_location() }
}

#[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
fn errno_location() -> *mut c_int {
    // SAFETY: as above.
    unsafe { libc::__errno() }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly"
))]
fn errno_location() -> *mut c_int {
    // SAFETY: as above.
    unsafe { libc::__error() }
}

#[cfg(target_os = "illumos")]
fn errno_location() -> *mut c_int {
    // SAFETY: as above.
    unsafe { libc::___errno() }
}

thread_local! {
    /// The shard this thread runs has a pump.
    static PUMPING: Cell<bool> = const { Cell::new(false) };
}

/// A pump on a runtime worker serves listeners polled where no shard runs.
static SERVICE_PUMP: AtomicBool = AtomicBool::new(false);

/// Makes sure a pump serves listeners polled here: one on the shard this
/// thread runs, or the one on a runtime worker.
fn ensure_pump() {
    let on_shard = crate::rt::context::current(|shard| shard.is_some());
    let started = if on_shard {
        PUMPING.try_with(|p| p.replace(true)).unwrap_or(true)
    } else {
        SERVICE_PUMP.swap(true, Ordering::SeqCst)
    };
    if started {
        return;
    }
    let made = wake().and_then(|wake| {
        // SAFETY: the wake descriptor is never closed, so the borrow is
        // good for the process.
        Handle::new(unsafe { BorrowedFd::borrow_raw(wake.read) })
    });
    match made {
        Ok(handle) => crate::rt::runtime::spawn_service(pump(handle, on_shard)),
        // No pump: the next poll tries again (a listener only misses its
        // wakes meanwhile, never a delivery, which its count keeps).
        Err(_) => pump_ended(on_shard),
    }
}

fn pump_ended(on_shard: bool) {
    if on_shard {
        let _ = PUMPING.try_with(|p| p.set(false));
    } else {
        SERVICE_PUMP.store(false, Ordering::SeqCst);
    }
}

/// Reads the wake descriptor, then dispatches, for as long as the shard
/// runs it.
async fn pump(wake: Handle<BorrowedFd<'static>>, on_shard: bool) {
    let mut buf = [0u8; 64];
    loop {
        let read = core::future::poll_fn(|cx| wake.source().poll_read(cx, &mut buf, false)).await;
        super::dispatch();
        if read.is_err() {
            break;
        }
    }
    pump_ended(on_shard);
}
