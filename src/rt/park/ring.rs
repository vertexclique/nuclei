//! The io_uring poller: the shard's ring, which every operation of the
//! shard's sources goes through, with a read of an eventfd kept armed on it
//! so that a wake from any thread ends a wait.
//!
//! The ring is made disabled, on whichever thread builds the shard, and
//! enabled by the shard's own thread before its first submission: a ring
//! with one issuer belongs to the thread that enables it. Entries that find
//! the submission queue full wait here, in order, and go in as room frees;
//! nothing is dropped and nothing blocks.

use core::mem::MaybeUninit;
use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use super::Notifier;
use crate::syscore::linux::iouring::ops::NOT_AN_OP;
use crate::syscore::linux::iouring::ring::sys::IORING_SETUP_SINGLE_ISSUER;
use crate::syscore::linux::iouring::ring::{IoUringCqe, Ring, RingConfig, SqFull, Sqe};

/// Where a wait puts the completions it reaps, and how many it may.
pub(crate) trait CqeSink {
    fn room(&self) -> usize;
    fn put(&mut self, cqe: IoUringCqe);
}

impl CqeSink for Vec<IoUringCqe> {
    fn room(&self) -> usize {
        usize::MAX
    }

    fn put(&mut self, cqe: IoUringCqe) {
        self.push(cqe);
    }
}

/// The most completions a [`CqeBatch`] holds: what one reap of a busy shard brings, on the stack.
pub(crate) const CQE_BATCH: usize = 32;

/// A fixed batch of completions, reaped in one go and handled after the ring is let go.
pub(crate) struct CqeBatch {
    cqes: [MaybeUninit<IoUringCqe>; CQE_BATCH],
    len: usize,
}

impl CqeBatch {
    pub(crate) fn new() -> Self {
        Self {
            cqes: [MaybeUninit::uninit(); CQE_BATCH],
            len: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Whether the batch took all it can, so more may wait in the ring.
    pub(crate) fn is_full(&self) -> bool {
        self.len == CQE_BATCH
    }

    /// The completion at `at`, which is below [`len`](Self::len).
    pub(crate) fn get(&self, at: usize) -> IoUringCqe {
        debug_assert!(at < self.len);
        // SAFETY: `put` initialized every slot below `len`.
        unsafe { self.cqes[at].assume_init() }
    }

    /// Empties the batch, its completions handled.
    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }
}

impl CqeSink for CqeBatch {
    fn room(&self) -> usize {
        CQE_BATCH - self.len
    }

    fn put(&mut self, cqe: IoUringCqe) {
        if let Some(slot) = self.cqes.get_mut(self.len) {
            slot.write(cqe);
            self.len += 1;
        }
    }
}

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
    /// The kernel held completions the queue had no room for, at the last
    /// look; how many times that began.
    overflowed: bool,
    overflows: u64,
    /// The last look that entered the kernel brought completions: the kernel may run only part of
    /// its task work in one enter, so the next look enters too.
    entered_with_more: bool,
}

// SAFETY: the ring is used by one thread at a time: the thread that runs
// its shard. A shard is built on one thread and moved to the thread that
// runs it before its first submission or wait; the ring is made disabled,
// and that first use enables it, which makes a single-issuer ring's running
// thread its one issuer. A ring that takes submissions from any thread (a
// worker shard that may be handed over, `rt::handoff`) moves with its shard
// at a hand-over, once its old runner touches it no more.
unsafe impl Send for RingPoller {}

impl RingPoller {
    /// A ring as `cfg` describes, disabled until [`enable`](Self::enable),
    /// with a table of `files` fixed files.
    pub(crate) fn new(cfg: &RingConfig, files: u32) -> io::Result<Self> {
        let mut cfg = *cfg;
        cfg.start_disabled = true;
        let ring = Ring::new(&cfg)?;
        if files > 0 {
            ring.register_files_sparse(files)?;
        }
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
            overflowed: false,
            overflows: 0,
            entered_with_more: false,
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
        // works. The registration is the calling thread's own, so a ring any
        // thread may submit to keeps none: its next runner could not use it,
        // and an old runner's would hold the ring open.
        if self.ring.flags() & IORING_SETUP_SINGLE_ISSUER != 0 {
            let _ = self.ring.register_ring_fd();
        }
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

    /// Hands the kernel every entry queued here, those waiting for room in
    /// the submission queue too, and has the kernel run the completion work
    /// it holds for this thread: what a source that goes asked of it (its
    /// cancels) is done before this returns. False when the kernel took not
    /// all of them (it holds completions it has no room for, or a kernel
    /// thread takes them later), which a later call tries again.
    pub(crate) fn submit_now(&mut self) -> io::Result<bool> {
        self.enable()?;
        loop {
            self.flush_waiting();
            match self.ring.submit() {
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if matches!(err.raw_os_error(), Some(libc::EBUSY | libc::EAGAIN)) => {
                    return Ok(false);
                }
                Err(err) => return Err(err),
            }
            if self.ring.sq_ready() > 0 {
                return Ok(false);
            }
            if self.waiting.is_empty() {
                return Ok(true);
            }
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
        out: &mut impl CqeSink,
    ) -> io::Result<bool> {
        if !self.enabled {
            self.enable()?;
        }
        self.arm_wake();
        self.flush_waiting();
        // The enter below flushes what the kernel holds into the queue as
        // room frees up.
        self.look_for_overflow();
        let looking = timeout == Some(Duration::ZERO);
        let mut entered = false;
        let result = if looking {
            self.ring
                .submit_or_look(self.entered_with_more)
                .map(|e| entered = e)
        } else {
            self.ring.submit_and_wait(1, timeout).map(drop)
        };
        match result {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EINTR) | Some(libc::EBUSY) | Some(libc::EAGAIN)
                ) => {}
            Err(err) => return Err(err),
        }
        let (woken, got) = self.reap(out);
        self.entered_with_more = entered && (woken || got > 0);
        self.look_for_overflow();
        // Completions freed room: entries that waited for it go in now.
        self.flush_waiting();
        Ok(woken)
    }

    /// Takes the completions that have arrived into `out`, as many as it has room for, without
    /// entering the kernel. Returns whether a wake came and how many completions it took
    /// (the wake not counted).
    pub(crate) fn reap(&mut self, out: &mut impl CqeSink) -> (bool, usize) {
        let mut woken = false;
        let mut got = 0;
        for cqe in self.ring.completions().take(out.room()) {
            if cqe.user_data == NOT_AN_OP {
                woken = true;
                self.wake_armed = false;
            } else {
                out.put(cqe);
                got += 1;
            }
        }
        (woken, got)
    }

    /// Counts an overflow the first time a look finds the kernel holding
    /// completions: before each enter, and after the completions are taken
    /// (one may begin while the wait sleeps, or after it ended).
    fn look_for_overflow(&mut self) {
        let overflowed = self.ring.cq_overflow_pending();
        self.overflows += u64::from(overflowed && !self.overflowed);
        self.overflowed = overflowed;
    }

    /// Puts the file of `fd` into fixed slot `slot` (-1 empties it).
    pub(crate) fn update_file(&self, slot: u32, fd: std::os::fd::RawFd) -> io::Result<()> {
        self.ring.update_file(slot, fd)
    }

    /// Times the kernel began holding completions the queue had no room
    /// for, as the waits saw.
    pub(crate) fn overflows(&self) -> u64 {
        self.overflows
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

    /// Floods `p` with `n` NOPs from `first`, submitted with no wait to take
    /// their completions: past the queue's room the kernel holds them.
    fn flood(p: &mut RingPoller, first: u64, n: u64) {
        let mut next = first;
        while next < first + n {
            while next < first + n && p.ring.push(&Sqe::nop().user_data(next)).is_ok() {
                next += 1;
            }
            match p.ring.submit() {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
                Err(e) => panic!("submit failed: {e}"),
            }
        }
        assert!(
            p.ring.cq_overflow_pending(),
            "the flood overflowed the queue"
        );
    }

    /// Completions past the queue's room wait in the kernel: the waits
    /// count that once however many of them it takes to take them all, and
    /// every completion comes.
    #[test]
    fn an_overflow_is_counted_once_and_loses_nothing() {
        let Some(mut p) = poller() else { return };
        const N: u64 = 64;
        let mut out = Vec::new();
        for round in 0..2 {
            flood(&mut p, round * N, N);
            assert_eq!(p.overflows(), round);
            out.clear();
            let deadline = Instant::now() + Duration::from_secs(10);
            while out.len() < N as usize {
                p.wait(Some(Duration::from_millis(100)), &mut out).unwrap();
                assert!(Instant::now() < deadline, "{} of {N} completed", out.len());
            }
            let mut got: Vec<u64> = out.iter().map(|c| c.user_data).collect();
            got.sort_unstable();
            assert_eq!(got, (round * N..round * N + N).collect::<Vec<_>>());
            assert_eq!(p.overflows(), round + 1);
        }
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

    /// A look with nothing to submit that enters the kernel never, as the ring's mode allows, still
    /// sees a wake and a read's completion that happen after it began: each mode a shard's ring is
    /// made in.
    #[test]
    fn a_look_sees_completions_that_arrive_while_it_only_looks() {
        use crate::syscore::linux::iouring::ring::taskrun::flag_works;
        let base = RingConfig::new(8);
        let modes = [
            (
                "deferred",
                RingConfig {
                    taskrun_flag: flag_works(),
                    ..base
                },
            ),
            ("deferred, no flag", base),
            ("handed over", base.handed_over()),
            (
                "kernel thread",
                RingConfig {
                    sqpoll_idle_ms: Some(50),
                    ..base
                },
            ),
        ];
        for (name, cfg) in modes {
            let mut p = match RingPoller::new(&cfg, 16) {
                Ok(p) => p,
                Err(err) => {
                    eprintln!("skipping {name}: {err}");
                    continue;
                }
            };
            p.enable().unwrap();
            let mut fds = [0; 2];
            // SAFETY: the kernel fills the two descriptors of our array.
            assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            let mut byte = Box::new(0u8);
            p.push(Sqe::read(fds[0], core::ptr::from_mut(&mut *byte), 1, u64::MAX).user_data(7));
            let mut out = Vec::new();
            // The first wait submits the read and arms the wake; nothing is left to submit after.
            assert!(!p.wait(Some(Duration::ZERO), &mut out).unwrap());
            let notifier = p.notifier();
            notifier.notify();
            // SAFETY: one byte of our own to our own pipe.
            assert_eq!(unsafe { libc::write(fds[1], [1u8].as_ptr().cast(), 1) }, 1);
            let (mut woken, deadline) = (false, Instant::now() + Duration::from_secs(10));
            while !woken || out.is_empty() {
                woken |= p.wait(Some(Duration::ZERO), &mut out).unwrap();
                assert!(
                    Instant::now() < deadline,
                    "{name}: woken {woken}, {} completions",
                    out.len()
                );
                std::thread::yield_now();
            }
            assert_eq!(out[0].user_data, 7, "{name}");
            // SAFETY: closes our own descriptors.
            unsafe { (libc::close(fds[0]), libc::close(fds[1])) };
        }
    }

    /// An idle ring whose kernel flags its task work is looked at without an enter, and enters
    /// once the flag is up.
    #[test]
    fn an_idle_look_makes_no_enter_where_the_kernel_flags_task_work() {
        use crate::syscore::linux::iouring::ring::taskrun::flag_works;
        if !flag_works() {
            eprintln!("skipping: this kernel does not flag task work");
            return;
        }
        let cfg = RingConfig {
            taskrun_flag: true,
            ..RingConfig::new(8)
        };
        let Ok(mut p) = RingPoller::new(&cfg, 16) else {
            return;
        };
        p.enable().unwrap();
        let mut out = Vec::new();
        p.wait(Some(Duration::ZERO), &mut out).unwrap();
        assert!(!p.ring.submit_or_look(false).unwrap());
        p.notifier().notify();
        assert!(p.ring.taskrun_pending());
        assert!(p.ring.submit_or_look(false).unwrap());
    }
}
