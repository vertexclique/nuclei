//! The Linux poller: an epoll instance with an eventfd for wakes.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

/// The epoll data of the wake eventfd; IO registrations never use it.
const WAKE: u64 = u64::MAX;

/// A shard's poller.
pub(crate) struct Poller {
    epoll: OwnedFd,
    wake: Arc<OwnedFd>,
    events: Vec<libc::epoll_event>,
}

/// Wakes a poller from any thread.
#[derive(Clone)]
pub(crate) struct Notifier {
    wake: Arc<OwnedFd>,
}

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

impl Poller {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: plain syscalls; every fd they return is owned right away.
        unsafe {
            let epoll = OwnedFd::from_raw_fd(check(libc::epoll_create1(libc::EPOLL_CLOEXEC))?);
            let wake = OwnedFd::from_raw_fd(check(libc::eventfd(
                0,
                libc::EFD_CLOEXEC | libc::EFD_NONBLOCK,
            ))?);
            let mut event = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: WAKE,
            };
            check(libc::epoll_ctl(
                epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                wake.as_raw_fd(),
                &mut event,
            ))?;
            Ok(Self {
                epoll,
                wake: Arc::new(wake),
                events: Vec::with_capacity(64),
            })
        }
    }

    pub(crate) fn notifier(&self) -> Notifier {
        Notifier {
            wake: Arc::clone(&self.wake),
        }
    }

    /// Blocks until a notifier fires or `timeout` passes (`None` waits for a
    /// wake, a zero timeout only looks). Returns whether a wake came.
    pub(crate) fn wait(&mut self, timeout: Option<Duration>) -> io::Result<bool> {
        // epoll counts milliseconds: round up, so a wait never ends early.
        let ms = match timeout {
            None => -1,
            Some(t) => {
                let ms = t.as_nanos().div_ceil(1_000_000);
                ms.min(libc::c_int::MAX as u128) as libc::c_int
            }
        };
        let capacity = self.events.capacity() as libc::c_int;
        // SAFETY: the buffer has room for `capacity` events; the kernel says
        // how many it wrote.
        let n = unsafe {
            let n = libc::epoll_wait(
                self.epoll.as_raw_fd(),
                self.events.as_mut_ptr(),
                capacity,
                ms,
            );
            if n < 0 {
                let err = io::Error::last_os_error();
                // A signal ended the wait: it returns like a timeout.
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
        for event in &self.events {
            if event.u64 == WAKE {
                woken = true;
            }
        }
        if woken {
            let mut count = 0u64;
            // Reset the counter. Nonblocking: a wake racing this read leaves it
            // nonzero, and the next wait returns at once, which is a spurious
            // wake and harmless.
            // SAFETY: reads 8 bytes into `count`.
            unsafe {
                libc::read(
                    self.wake.as_raw_fd(),
                    (&mut count as *mut u64).cast(),
                    size_of::<u64>(),
                )
            };
        }
        Ok(woken)
    }
}

impl Notifier {
    /// Wakes the poller, or makes its next wait return at once.
    pub(crate) fn notify(&self) {
        let one = 1u64;
        // SAFETY: writes 8 bytes. The only failure is a full counter
        // (EAGAIN), which already wakes the poller.
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                (&one as *const u64).cast(),
                size_of::<u64>(),
            )
        };
    }
}
