//! The kqueue poller (macOS and the BSDs): an `EVFILT_USER` event for wakes,
//! the registrations of the shard's IO sources, and an `EVFILT_TIMER` in
//! nanoseconds that ends a wait at a precise deadline where the system has
//! one (the wait's own timeout may be coalesced with other timers).

// The kevent fields and flag constants have different types on different
// BSDs; the casts that make one source fit them all are no-ops on some.
#![allow(clippy::unnecessary_cast)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Readiness;

/// The ident of the wake event; IO registrations use file descriptors.
const WAKE: libc::uintptr_t = 0;
/// The ident of the precise deadline's timer (`EVFILT_TIMER` idents are
/// their own namespace).
#[cfg(precise_timer)]
const TIMER: libc::uintptr_t = 0;

/// A shard's poller.
pub(crate) struct Poller {
    queue: Arc<OwnedFd>,
    events: Vec<libc::kevent>,
    /// The timer change the next wait submits, for a precise deadline.
    #[cfg(precise_timer)]
    timer: Option<libc::kevent>,
    /// The deadline the timer is armed for, until it fires.
    #[cfg(precise_timer)]
    armed_for: Option<Instant>,
}

// SAFETY: the event buffer is plain data the kernel fills on each wait; its
// `udata` pointers are never dereferenced (nuclei stores none), so the
// poller may move to the thread that runs its shard.
unsafe impl Send for Poller {}

/// Wakes a poller from any thread.
#[derive(Clone)]
pub(crate) struct Notifier {
    queue: Arc<OwnedFd>,
}

/// Adds and removes registrations of a poller, from any thread.
#[derive(Clone)]
pub(crate) struct Registrar {
    queue: Arc<OwnedFd>,
}

/// A kevent for the wake event, built from a zeroed struct so the fields that
/// differ between the BSDs stay zero.
fn wake_event(flags: u16, fflags: u32) -> libc::kevent {
    // SAFETY: kevent is plain data; all zeroes is a valid value.
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    event.ident = WAKE;
    event.filter = libc::EVFILT_USER as _;
    event.flags = flags as _;
    event.fflags = fflags as _;
    event
}

/// A kevent for `fd`'s `filter`, built from a zeroed struct so the fields
/// that differ between the BSDs stay zero.
fn fd_event(fd: RawFd, filter: i16, flags: u16, token: u64) -> libc::kevent {
    // SAFETY: kevent is plain data; all zeroes is a valid value.
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    event.ident = fd as _;
    event.filter = filter as _;
    event.flags = flags as _;
    event.udata = token as usize as _;
    event
}

/// The change for the exit of child `pid`.
fn exit_event(pid: u32, flags: impl Into<u32>, token: u64) -> libc::kevent {
    // SAFETY: kevent is plain data; all zeroes is a valid value.
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    event.ident = pid as _;
    event.filter = libc::EVFILT_PROC as _;
    event.flags = flags.into() as _;
    event.fflags = libc::NOTE_EXIT as _;
    event.udata = token as usize as _;
    event
}

/// The change that watches `fd` for urgent data: Apple's `EVFILT_EXCEPT`
/// with `NOTE_OOB` (its read filter does not report it).
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn urgent_event(fd: RawFd, flags: u16, token: u64) -> libc::kevent {
    let mut event = fd_event(fd, libc::EVFILT_EXCEPT, flags, token);
    event.fflags = libc::NOTE_OOB;
    event
}

fn submit(queue: &OwnedFd, event: &libc::kevent) -> io::Result<()> {
    submit_all(queue, core::slice::from_ref(event))
}

fn submit_all(queue: &OwnedFd, events: &[libc::kevent]) -> io::Result<()> {
    // SAFETY: the changes are read from `events`; no event out.
    let ret = unsafe {
        libc::kevent(
            queue.as_raw_fd(),
            events.as_ptr(),
            events.len() as _,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Poller {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: a plain syscall; the fd is owned right away.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a new descriptor nobody else owns.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: sets close-on-exec on our own descriptor.
        unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        // Registered once; EV_CLEAR resets it each time it is delivered.
        submit(&queue, &wake_event((libc::EV_ADD | libc::EV_CLEAR) as _, 0))?;
        Ok(Self {
            queue: Arc::new(queue),
            events: Vec::with_capacity(64),
            #[cfg(precise_timer)]
            timer: None,
            #[cfg(precise_timer)]
            armed_for: None,
        })
    }

    /// Makes the next wait end at the precise deadline `at`, `after` from
    /// now: a one-shot `EVFILT_TIMER` in nanoseconds (on Apple's systems
    /// marked critical, so it is not coalesced), submitted with the wait
    /// itself, so it costs no call of its own. Nothing to do for the deadline
    /// the timer is armed for already.
    #[cfg(precise_timer)]
    pub(crate) fn arm_precise(&mut self, at: Instant, after: Duration) {
        if self.armed_for == Some(at) {
            return;
        }
        // SAFETY: kevent is plain data; all zeroes is a valid value.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        event.ident = TIMER;
        event.filter = libc::EVFILT_TIMER as _;
        event.flags = (libc::EV_ADD | libc::EV_ONESHOT) as _;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let fflags = libc::NOTE_NSECONDS | libc::NOTE_CRITICAL;
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let fflags = libc::NOTE_NSECONDS;
        event.fflags = fflags as _;
        event.data = after.as_nanos().min(i64::MAX as u128) as _;
        self.timer = Some(event);
        self.armed_for = Some(at);
    }

    /// The wait's own timeout counts nanoseconds here, and no timer filter
    /// counts finer: it ends a wait at a precise deadline by itself.
    #[cfg(not(precise_timer))]
    pub(crate) fn arm_precise(&mut self, _at: Instant, _after: Duration) {}

    pub(crate) fn notifier(&self) -> Notifier {
        Notifier {
            queue: Arc::clone(&self.queue),
        }
    }

    pub(crate) fn registrar(&self) -> Registrar {
        Registrar {
            queue: Arc::clone(&self.queue),
        }
    }

    /// Blocks until a notifier fires, a registered source is ready, or
    /// `timeout` passes (`None` waits, a zero timeout only looks). Calls
    /// `on_event` with the token and readiness of each ready source. Returns
    /// whether a wake came.
    #[cfg(test)]
    pub(crate) fn wait(
        &mut self,
        timeout: Option<Duration>,
        mut on_event: impl FnMut(u64, Readiness),
    ) -> io::Result<bool> {
        self.wait_at_most(usize::MAX, timeout, &mut on_event)
    }

    /// Blocks until a notifier fires, a registered source is ready, or
    /// `timeout` passes (`None` waits, a zero timeout only looks), taking at
    /// most `max` events (at least one) from the kernel. Calls `on_event`
    /// with the token and readiness of each ready source. Returns whether a
    /// wake came.
    pub(crate) fn wait_at_most(
        &mut self,
        max: usize,
        timeout: Option<Duration>,
        mut on_event: impl FnMut(u64, Readiness),
    ) -> io::Result<bool> {
        let ts = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
            tv_nsec: t.subsec_nanos() as _,
        });
        let ts_ptr = ts
            .as_ref()
            .map_or(std::ptr::null(), |t| t as *const libc::timespec);
        #[cfg(precise_timer)]
        let change = self.timer.take();
        #[cfg(precise_timer)]
        let (changes, nchanges) = change
            .as_ref()
            .map_or((std::ptr::null(), 0), |c| (c as *const libc::kevent, 1));
        #[cfg(not(precise_timer))]
        let (changes, nchanges) = (std::ptr::null(), 0);
        // SAFETY: the buffer has room for its capacity; the kernel says how
        // many it wrote. The change, if any, is a local that outlives the call.
        let n = unsafe {
            let n = libc::kevent(
                self.queue.as_raw_fd(),
                changes,
                nchanges,
                self.events.as_mut_ptr(),
                self.events.capacity().min(max).max(1) as _,
                ts_ptr,
            );
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    0
                } else {
                    return Err(err);
                }
            } else {
                n as usize
            }
        };
        // SAFETY: the kernel wrote `n` events.
        unsafe { self.events.set_len(n) };
        let mut woken = false;
        for e in &self.events {
            if e.filter == libc::EVFILT_USER as _ {
                woken |= e.ident == WAKE;
                continue;
            }
            #[cfg(precise_timer)]
            if e.filter == libc::EVFILT_TIMER as _ {
                // Fired, or refused (an `EV_ERROR` for the change): either way
                // nothing is armed now, and the wait's own timeout still held
                // the deadline.
                if u64::from(e.flags) & u64::from(libc::EV_ERROR) != 0 {
                    log::warn!(
                        "a precise sleep ends on the wait's own timeout: its timer was refused: {}",
                        io::Error::from_raw_os_error(e.data as i32)
                    );
                }
                self.armed_for = None;
                continue;
            }
            // The flags are 16 bits wide on most BSDs and 32 on NetBSD.
            let done = u64::from(e.flags) & u64::from(libc::EV_EOF | libc::EV_ERROR) != 0;
            let urgent = urgent(e);
            on_event(
                e.udata as usize as u64,
                Readiness {
                    readable: e.filter == libc::EVFILT_READ as _ || done || urgent,
                    writable: e.filter == libc::EVFILT_WRITE as _ || done,
                    urgent,
                    hangup: done,
                },
            );
        }
        Ok(woken)
    }
}

/// Whether an event says urgent data came (Apple's `EVFILT_EXCEPT` with
/// `NOTE_OOB`; the other kqueues do not say, and nuclei never takes a short
/// read as draining there).
fn urgent(e: &libc::kevent) -> bool {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        e.filter == libc::EVFILT_EXCEPT as _ && e.fflags & libc::NOTE_OOB != 0
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        let _ = e;
        false
    }
}

impl Registrar {
    /// Registers `fd` for readiness to read and to write, edge-triggered
    /// (EV_CLEAR), reported with `token`; `urgent`: and for urgent data (a
    /// TCP socket), where the kqueue reports it.
    pub(crate) fn add(&self, fd: RawFd, token: u64, urgent: bool) -> io::Result<()> {
        let flags = (libc::EV_ADD | libc::EV_CLEAR) as u16;
        let rw = [
            fd_event(fd, libc::EVFILT_READ as i16, flags, token),
            fd_event(fd, libc::EVFILT_WRITE as i16, flags, token),
        ];
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if urgent {
            return submit_all(&self.queue, &[rw[0], rw[1], urgent_event(fd, flags, token)]);
        }
        let _ = urgent;
        submit_all(&self.queue, &rw)
    }

    /// Watches the exit of child `pid`, reported once with `token`; false
    /// when the child exited already (no live process has the pid).
    pub(crate) fn add_exit(&self, pid: u32, token: u64) -> io::Result<bool> {
        match submit(
            &self.queue,
            &exit_event(pid, libc::EV_ADD | libc::EV_ONESHOT, token),
        ) {
            Ok(()) => Ok(true),
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Ends the exit watch of child `pid`, if it did not fire.
    pub(crate) fn delete_exit(&self, pid: u32) {
        // A watch that fired is gone already: nothing to remove.
        let _ = submit(&self.queue, &exit_event(pid, libc::EV_DELETE, 0));
    }

    /// Removes the registration of `fd`, made with `urgent`. Called before
    /// `fd` closes.
    pub(crate) fn delete(&self, fd: RawFd, urgent: bool) -> io::Result<()> {
        let flags = libc::EV_DELETE as u16;
        let rw = [
            fd_event(fd, libc::EVFILT_READ as i16, flags, 0),
            fd_event(fd, libc::EVFILT_WRITE as i16, flags, 0),
        ];
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if urgent {
            return submit_all(&self.queue, &[rw[0], rw[1], urgent_event(fd, flags, 0)]);
        }
        let _ = urgent;
        submit_all(&self.queue, &rw)
    }
}

impl Notifier {
    /// Wakes the poller, or makes its next wait return at once.
    pub(crate) fn notify(&self) {
        // A failure here means the queue is gone, so nobody waits on it.
        let _ = submit(&self.queue, &wake_event(0, libc::NOTE_TRIGGER));
    }
}
