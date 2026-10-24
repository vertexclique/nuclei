//! The kqueue poller (macOS and the BSDs): an `EVFILT_USER` event for wakes.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

/// The ident of the wake event; IO registrations use file descriptors.
const WAKE: libc::uintptr_t = 0;

/// A shard's poller.
pub(crate) struct Poller {
    queue: Arc<OwnedFd>,
    events: Vec<libc::kevent>,
}

/// Wakes a poller from any thread.
#[derive(Clone)]
pub(crate) struct Notifier {
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

fn submit(queue: &OwnedFd, event: &libc::kevent) -> io::Result<()> {
    // SAFETY: one change, no event out.
    let ret = unsafe {
        libc::kevent(
            queue.as_raw_fd(),
            event,
            1,
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
        submit(&queue, &wake_event(libc::EV_ADD | libc::EV_CLEAR, 0))?;
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

    /// Blocks until a notifier fires or `timeout` passes (`None` waits for a
    /// wake, a zero timeout only looks). Returns whether a wake came.
    pub(crate) fn wait(&mut self, timeout: Option<Duration>) -> io::Result<bool> {
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
                self.events.capacity() as _,
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
        Ok(self
            .events
            .iter()
            .any(|e| e.filter == libc::EVFILT_USER as _ && e.ident == WAKE))
    }
}

impl Notifier {
    /// Wakes the poller, or makes its next wait return at once.
    pub(crate) fn notify(&self) {
        // A failure here means the queue is gone, so nobody waits on it.
        let _ = submit(&self.queue, &wake_event(0, libc::NOTE_TRIGGER));
    }
}
