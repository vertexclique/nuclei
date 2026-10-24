//! The Linux poller: an epoll instance with an eventfd for wakes, and the
//! registrations of the shard's IO sources.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;

use super::Readiness;

/// The epoll data of the wake eventfd; IO registrations never use it.
const WAKE: u64 = u64::MAX;

/// A shard's poller.
pub(crate) struct Poller {
    epoll: Arc<OwnedFd>,
    wake: Arc<OwnedFd>,
    events: Vec<libc::epoll_event>,
}

/// Wakes a poller from any thread.
#[derive(Clone)]
pub(crate) struct Notifier {
    wake: Arc<OwnedFd>,
}

/// Adds and removes registrations of a poller, from any thread.
#[derive(Clone)]
pub(crate) struct Registrar {
    epoll: Arc<OwnedFd>,
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
            // Edge-triggered: every write is one event, and the counter is
            // never read back, which saves a syscall per wake.
            let mut event = libc::epoll_event {
                events: (libc::EPOLLIN | libc::EPOLLET) as u32,
                u64: WAKE,
            };
            check(libc::epoll_ctl(
                epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                wake.as_raw_fd(),
                &mut event,
            ))?;
            Ok(Self {
                epoll: Arc::new(epoll),
                wake: Arc::new(wake),
                events: Vec::with_capacity(256),
            })
        }
    }

    pub(crate) fn notifier(&self) -> Notifier {
        Notifier {
            wake: Arc::clone(&self.wake),
        }
    }

    pub(crate) fn registrar(&self) -> Registrar {
        Registrar {
            epoll: Arc::clone(&self.epoll),
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
        // epoll counts milliseconds: round up, so a wait never ends early.
        let ms = match timeout {
            None => -1,
            Some(t) => {
                let ms = t.as_nanos().div_ceil(1_000_000);
                ms.min(libc::c_int::MAX as u128) as libc::c_int
            }
        };
        let capacity = self.events.capacity().min(max).max(1) as libc::c_int;
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
            let token = event.u64;
            if token == WAKE {
                woken = true;
                continue;
            }
            let bits = event.events as libc::c_int;
            let hup = bits & (libc::EPOLLHUP | libc::EPOLLERR | libc::EPOLLRDHUP) != 0;
            let urgent = bits & libc::EPOLLPRI != 0;
            on_event(
                token,
                Readiness {
                    readable: bits & libc::EPOLLIN != 0 || hup || urgent,
                    writable: bits & libc::EPOLLOUT != 0
                        || bits & (libc::EPOLLHUP | libc::EPOLLERR) != 0,
                    urgent,
                    hangup: hup,
                },
            );
        }
        Ok(woken)
    }
}

impl Notifier {
    /// The notifier of a poller waiting on the eventfd `wake`.
    #[cfg(feature = "iouring")]
    pub(crate) fn of(wake: Arc<OwnedFd>) -> Self {
        Self { wake }
    }

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

impl Registrar {
    /// Registers `fd` for readiness to read and to write, edge-triggered,
    /// reported with `token`; `urgent`: and for urgent data (a TCP socket).
    pub(crate) fn add(&self, fd: RawFd, token: u64, urgent: bool) -> io::Result<()> {
        let pri = if urgent { libc::EPOLLPRI } else { 0 };
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | pri | libc::EPOLLOUT | libc::EPOLLRDHUP | libc::EPOLLET)
                as u32,
            u64: token,
        };
        // SAFETY: a plain syscall; `fd` is the caller's open descriptor.
        check(unsafe {
            libc::epoll_ctl(self.epoll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut event)
        })
        .map(drop)
    }

    /// Removes the registration of `fd`, made with `urgent` (one removal
    /// takes every event here). Called before `fd` closes.
    pub(crate) fn delete(&self, fd: RawFd, _urgent: bool) -> io::Result<()> {
        let mut event = libc::epoll_event { events: 0, u64: 0 };
        // SAFETY: a plain syscall; `fd` is still open.
        check(unsafe {
            libc::epoll_ctl(self.epoll.as_raw_fd(), libc::EPOLL_CTL_DEL, fd, &mut event)
        })
        .map(drop)
    }
}
