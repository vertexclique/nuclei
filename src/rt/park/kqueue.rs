//! The kqueue poller (macOS and the BSDs): an `EVFILT_USER` event for wakes,
//! and the registrations of the shard's IO sources.

// The kevent fields and flag constants have different types on different
// BSDs; the casts that make one source fit them all are no-ops on some.
#![allow(clippy::unnecessary_cast)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use super::Readiness;

/// The ident of the wake event; IO registrations use file descriptors.
const WAKE: libc::uintptr_t = 0;

/// A shard's poller.
pub(crate) struct Poller {
    queue: Arc<OwnedFd>,
    events: Vec<libc::kevent>,
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
        })
    }

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
        // SAFETY: the buffer has room for its capacity; the kernel says how
        // many it wrote.
        let n = unsafe {
            let n = libc::kevent(
                self.queue.as_raw_fd(),
                std::ptr::null(),
                0,
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
