//! The Linux poller: an epoll instance with an eventfd for wakes, the
//! registrations of the shard's IO sources, and a timerfd that ends a wait at
//! a precise deadline (epoll's own timeout counts milliseconds).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Readiness;

/// The epoll data of the wake eventfd; IO registrations never use it.
const WAKE: u64 = u64::MAX;
/// The epoll data of the precise timer's timerfd: a token no registration
/// reaches, as for [`WAKE`] (it would take 2^32 live sources).
const TIMER: u64 = u64::MAX - 1;

/// A shard's poller.
pub(crate) struct Poller {
    epoll: Arc<OwnedFd>,
    wake: Arc<OwnedFd>,
    events: Vec<libc::epoll_event>,
    /// The timerfd of the precise deadlines, made on the first one.
    timer: Timer,
}

/// The poller's timerfd.
enum Timer {
    /// No precise deadline came yet.
    Unmade,
    /// Made and registered; armed for that deadline until it fires.
    Made {
        fd: OwnedFd,
        armed_for: Option<Instant>,
    },
    /// It could not be made: precise deadlines end on epoll's millisecond.
    Failed,
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
                timer: Timer::Unmade,
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

    /// Makes the next wait end at the precise deadline `at`, `after` from
    /// now, through the timerfd: the wait's own timeout rounds it up to the
    /// millisecond. One system call when the deadline is new; none for the
    /// deadline the timer is armed for already. A timer that cannot be made
    /// leaves the deadline to that timeout, a millisecond late at most, and
    /// says so once.
    pub(crate) fn arm_precise(&mut self, at: Instant, after: Duration) {
        if let Timer::Unmade = self.timer {
            self.timer = match self.make_timer() {
                Ok(fd) => Timer::Made {
                    fd,
                    armed_for: None,
                },
                Err(err) => {
                    log::warn!(
                        "precise sleeps on this shard end on the millisecond: its timer could not be made: {err}"
                    );
                    Timer::Failed
                }
            };
        }
        let Timer::Made { fd, armed_for } = &mut self.timer else {
            return;
        };
        if *armed_for == Some(at) {
            return;
        }
        // A zero time disarms a timerfd: a deadline that is due now gets the
        // shortest one.
        let after = after.max(Duration::from_nanos(1));
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: after.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
                tv_nsec: after.subsec_nanos() as _,
            },
        };
        // SAFETY: a plain syscall on our own descriptor with a valid spec.
        let ret = unsafe { libc::timerfd_settime(fd.as_raw_fd(), 0, &spec, core::ptr::null_mut()) };
        if ret == 0 {
            *armed_for = Some(at);
        } else {
            *armed_for = None;
            log::warn!(
                "a precise sleep ends on the millisecond: its timer could not be armed: {}",
                io::Error::last_os_error()
            );
        }
    }

    /// The timerfd of the precise deadlines, registered edge-triggered: each
    /// expiry is one event, and nothing reads the count back.
    fn make_timer(&self) -> io::Result<OwnedFd> {
        // SAFETY: plain syscalls; the descriptor is owned at once.
        unsafe {
            let fd = OwnedFd::from_raw_fd(check(libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
            ))?);
            let mut event = libc::epoll_event {
                events: (libc::EPOLLIN | libc::EPOLLET) as u32,
                u64: TIMER,
            };
            check(libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                fd.as_raw_fd(),
                &mut event,
            ))?;
            Ok(fd)
        }
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
                let ms = t
                    .as_secs()
                    .saturating_mul(1000)
                    .saturating_add(u64::from(t.subsec_nanos()).div_ceil(1_000_000));
                ms.min(libc::c_int::MAX as u64) as libc::c_int
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
            if token == TIMER {
                if let Timer::Made { armed_for, .. } = &mut self.timer {
                    *armed_for = None;
                }
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

    /// Registers a child's `pidfd` for its exit, reported once with `token`.
    pub(crate) fn add_exit(&self, pidfd: RawFd, token: u64) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLONESHOT) as u32,
            u64: token,
        };
        // SAFETY: a plain syscall; `pidfd` is the child's open pidfd.
        check(unsafe {
            libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                pidfd,
                &mut event,
            )
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
