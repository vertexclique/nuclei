//! The io_uring poller: the shard's ring, which every operation of the
//! shard's sources goes through, with a read of an eventfd kept armed on it
//! so that a wake from any thread ends a wait.
//!
//! The ring is made disabled, on whichever thread builds the shard, and
//! enabled by the shard's own thread before its first submission: a ring
//! with one issuer belongs to the thread that enables it. Entries that find
//! the submission queue full wait here, in order, and go in as room frees;
//! nothing is dropped and nothing blocks.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use super::Notifier;
use crate::syscore::linux::iouring::ops::NOT_AN_OP;
use crate::syscore::linux::iouring::ring::{IoUringCqe, Ring, RingConfig, SqFull, Sqe};

/// A shard's ring.
pub(crate) struct RingPoller {
    ring: Ring,
    wake: Arc<OwnedFd>,
    /// Where the armed read of the eventfd lands; boxed, so it stays put
    /// while the read is out.
    wake_buf: Box<u64>,
    wake_armed: bool,
    /// Entries that found the submission queue full, in order.
    waiting: VecDeque<Sqe>,
    /// The shard's thread made itself the issuer.
    enabled: bool,
}

// SAFETY: the ring is used by one thread only. A shard is built on one
// thread and moved to the thread that runs it before its first submission
// or wait; the ring is made disabled, and that first use enables it, which
// makes the running thread its one issuer. The thread that built it never
// touches it after the move.
unsafe impl Send for RingPoller {}

impl RingPoller {
    /// A ring as `cfg` describes, disabled until [`enable`](Self::enable),
    /// with a table of `files` fixed files.
    pub(crate) fn new(cfg: &RingConfig, files: u32) -> io::Result<Self> {
        let mut cfg = *cfg;
        cfg.start_disabled = true;
        let ring = Ring::new(&cfg)?;
        ring.register_files_sparse(files)?;
        // SAFETY: a plain syscall; the descriptor it returns is owned at once.
        let wake = unsafe {
            let fd = libc::eventfd(0, libc::EFD_CLOEXEC);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(fd)
        };
        Ok(Self {
            ring,
            wake: Arc::new(wake),
            wake_buf: Box::new(0),
            wake_armed: false,
            waiting: VecDeque::new(),
            enabled: false,
        })
    }

    /// Makes the calling thread the ring's issuer, on its first call: only
    /// the shard's own thread submits and waits.
    pub(crate) fn enable(&mut self) -> io::Result<()> {
        if self.enabled {
            return Ok(());
        }
        self.ring.enable()?;
        self.enabled = true;
        // One fd lookup less per enter; a ring that cannot register it still
        // works.
        let _ = self.ring.register_ring_fd();
        Ok(())
    }

    pub(crate) fn notifier(&self) -> Notifier {
        Notifier::of(Arc::clone(&self.wake))
    }

    /// Queues `sqe`: into the submission queue if it has room, else behind
    /// the entries waiting for room. The next wait submits it.
    pub(crate) fn push(&mut self, sqe: Sqe) {
        if !self.waiting.is_empty() || self.ring.push(&sqe).is_err() {
            self.waiting.push_back(sqe);
        }
    }

    /// Moves waiting entries into the submission queue while it has room.
    fn flush_waiting(&mut self) {
        while let Some(sqe) = self.waiting.front() {
            if let Err(SqFull) = self.ring.push(sqe) {
                return;
            }
            self.waiting.pop_front();
        }
    }

    fn arm_wake(&mut self) {
        if self.wake_armed {
            return;
        }
        let sqe = Sqe::read(
            self.wake.as_raw_fd(),
            core::ptr::from_mut(&mut *self.wake_buf).cast(),
            8,
            0,
        )
        .user_data(NOT_AN_OP);
        // The wake goes in first, so a full queue never leaves the shard
        // unwakeable.
        if self.ring.push(&sqe).is_err() {
            self.waiting.push_front(sqe);
        }
        self.wake_armed = true;
    }

    /// Submits what is queued and waits up to `timeout` (`None`: until a
    /// completion or a wake comes; zero: only looks) for completions, which
    /// go to `out`. Returns whether a wake came.
    pub(crate) fn wait(
        &mut self,
        timeout: Option<Duration>,
        out: &mut Vec<IoUringCqe>,
    ) -> io::Result<bool> {
        self.enable()?;
        self.arm_wake();
        self.flush_waiting();
        let min = u32::from(timeout != Some(Duration::ZERO));
        match self.ring.submit_and_wait(min, timeout) {
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EINTR) | Some(libc::EBUSY) | Some(libc::EAGAIN)
                ) => {}
            Err(err) => return Err(err),
        }
        let mut woken = false;
        for cqe in self.ring.completions() {
            if cqe.user_data == NOT_AN_OP {
                woken = true;
                self.wake_armed = false;
            } else {
                out.push(cqe);
            }
        }
        // Completions freed room: entries that waited for it go in now.
        self.flush_waiting();
        Ok(woken)
    }

    /// Puts the file of `fd` into fixed slot `slot` (-1 empties it).
    pub(crate) fn update_file(&self, slot: u32, fd: std::os::fd::RawFd) -> io::Result<()> {
        self.ring.update_file(slot, fd)
    }

    /// The ring's completion queue size: the bound of operations in flight.
    pub(crate) fn cq_entries(&self) -> u32 {
        self.ring.cq_entries()
    }

    /// The ring itself, to register a buffer group on it before its first
    /// submission.
    pub(crate) fn ring(&self) -> &Ring {
        &self.ring
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::RingPoller;
    use crate::syscore::linux::iouring::ring::{RingConfig, Sqe};

    fn poller() -> Option<RingPoller> {
        let mut p = match RingPoller::new(&RingConfig::new(8), 16) {
            Ok(p) => p,
            Err(err) => {
                eprintln!("skipping: io_uring unavailable: {err}");
                return None;
            }
        };
        p.enable().unwrap();
        Some(p)
    }

    #[test]
    fn a_wake_from_another_thread_ends_a_wait() {
        let Some(mut p) = poller() else { return };
        let n = p.notifier();
        let waker = std::thread::spawn(move || n.notify());
        let mut out = Vec::new();
        let start = Instant::now();
        loop {
            if p.wait(Some(Duration::from_secs(10)), &mut out).unwrap() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "no wake came");
        }
        waker.join().unwrap();
        assert!(out.is_empty());
        // Re-armed: a second wake ends a second wait.
        p.notifier().notify();
        assert!(p.wait(Some(Duration::from_secs(10)), &mut out).unwrap());
    }

    #[test]
    fn a_timed_wait_ends_at_its_timeout() {
        let Some(mut p) = poller() else { return };
        let mut out = Vec::new();
        let start = Instant::now();
        assert!(!p.wait(Some(Duration::from_millis(20)), &mut out).unwrap());
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    /// More entries than the submission queue holds: they wait in order
    /// and all complete.
    #[test]
    fn entries_past_a_full_queue_wait_and_complete_in_order() {
        let Some(mut p) = poller() else { return };
        for i in 0..40u64 {
            p.push(Sqe::nop().user_data(i));
        }
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while out.len() < 40 {
            p.wait(Some(Duration::from_millis(100)), &mut out).unwrap();
            assert!(Instant::now() < deadline, "{} of 40 completed", out.len());
        }
        let got: Vec<u64> = out.iter().map(|c| c.user_data).collect();
        assert_eq!(got, (0..40).collect::<Vec<_>>());
    }
}
