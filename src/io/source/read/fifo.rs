//! The end of a named FIFO on Apple systems (issue #58).
//!
//! kqueue never reports that a FIFO's writer closed once its reader drained it: XNU's FIFO read
//! filter fires only while bytes are there and never sets `EV_EOF`, and the close does not
//! trigger it (`poll` is as silent; only a `read` returns 0). Go keeps FIFOs out of kqueue on
//! darwin for it (golang/go#24164). A side that waits for its next edge after its reads drained a
//! FIFO would wait for good, so here it also looks again on a timer of its own: 1 ms after a task
//! waits, twice as long after each look that finds nothing, up to 64 ms, and from 1 ms again
//! once an edge brings bytes. Bytes still come at the edge; only the end is seen late, by at most
//! the period the side waits. An anonymous pipe, which kqueue reports the end of, is left out:
//! only a FIFO on a file system answers `F_GETPATH`.

use core::pin::Pin;
use core::task::{Context, Waker};
use std::os::fd::RawFd;
use std::time::Duration;

use crate::time::{Instant, Sleep};

/// The wait before the first look after the side blocked.
const FIRST: Duration = Duration::from_millis(1);
/// The longest wait between two looks.
const LAST: Duration = Duration::from_millis(64);
/// `MAXPATHLEN` on Apple systems: the buffer `F_GETPATH` fills.
const PATH: usize = 1024;

/// A side's looks at a FIFO the poller will not say the end of.
#[derive(Default)]
pub(super) struct FifoLook {
    /// Whether the descriptor is a named FIFO: found the first time a task waits on the side.
    fifo: Option<bool>,
    /// The wait before the next look; zero for the first.
    period: Duration,
    /// The look's timer, kept for the next one.
    timer: Option<Pin<Box<Sleep>>>,
    /// The timer is armed for the next look.
    armed: bool,
}

impl FifoLook {
    /// Whether the armed look's time came: the side reads again now, and the next look, if this
    /// one finds nothing, waits twice as long.
    #[inline(always)]
    pub(super) fn due(&mut self) -> bool {
        if !self.armed || !self.timer.as_ref().is_some_and(|timer| timer.is_elapsed()) {
            return false;
        }
        self.armed = false;
        self.period = (self.period * 2).min(LAST);
        true
    }

    /// A task waits on the side of `fd` for an edge: for a named FIFO, the next look is armed if
    /// none is, and its timer holds `waker`. Never for a descriptor that is not one.
    pub(super) fn hold(&mut self, fd: RawFd, waker: &Waker) {
        if !*self.fifo.get_or_insert_with(|| is_named_fifo(fd)) {
            return;
        }
        if !self.armed {
            self.period = self.period.max(FIRST);
            let deadline = Instant::now() + self.period;
            match &mut self.timer {
                Some(timer) => timer.as_mut().reset(deadline),
                None => self.timer = Some(Box::pin(crate::time::sleep_until(deadline))),
            }
            self.armed = true;
        }
        // A look whose time came since the side read wakes the task at once.
        if let Some(timer) = &mut self.timer
            && timer
                .as_mut()
                .poll(&mut Context::from_waker(waker))
                .is_ready()
        {
            waker.wake_by_ref();
        }
    }

    /// An edge came: the side's next wait looks first after the shortest period again.
    pub(super) fn settle(&mut self) {
        self.period = Duration::ZERO;
        self.armed = false;
    }
}

/// Whether `fd` is a FIFO on a file system: a FIFO by its type, and a vnode (`F_GETPATH`
/// answers only for one, never for an anonymous pipe).
fn is_named_fifo(fd: RawFd) -> bool {
    // SAFETY: plain data; the call fills it.
    let mut stat: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: a descriptor of the source's, and a place for its status.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 || stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
        return false;
    }
    let mut path = [0u8; PATH];
    // SAFETY: `path` holds the `MAXPATHLEN` bytes `F_GETPATH` writes at most.
    unsafe { libc::fcntl(fd, libc::F_GETPATH, path.as_mut_ptr()) == 0 }
}
