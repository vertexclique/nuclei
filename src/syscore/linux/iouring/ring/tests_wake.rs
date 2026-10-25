//! Wakes between rings, measured: what a shard's wake of another costs.
//!
//! Two threads, each with a ring made as a shard's is (one issuer, deferred
//! completion work), wake each other in turn, each parked in its ring's
//! wait in between. Three ways: a write to an eventfd whose read is armed
//! on the parked ring and armed again after each wake (what the shards do
//! now); a message from the waker's own ring, submitted at once, then a
//! wait of its own; and that message submitted by the very enter that waits.

use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use super::tests::ring_or_skip;
use super::*;

/// The user data of a wake on the parked ring.
const WAKE: u64 = 1;
/// The user data of the armed eventfd read.
const READ: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Via {
    Eventfd,
    MsgNow,
    MsgFolded,
}

/// A thread's end: its ring, its eventfd (for the eventfd way) and where
/// the armed read lands.
struct End {
    ring: Ring,
    efd: OwnedFd,
    buf: Box<u64>,
}

impl End {
    fn new() -> Option<Self> {
        let ring = ring_or_skip()?;
        // SAFETY: a plain syscall; the descriptor is owned at once.
        let efd = unsafe { OwnedFd::from_raw_fd(libc::eventfd(0, libc::EFD_CLOEXEC)) };
        Some(Self {
            ring,
            efd,
            buf: Box::new(0),
        })
    }

    fn arm(&mut self) {
        let sqe = Sqe::read(
            self.efd.as_raw_fd(),
            core::ptr::from_mut(&mut *self.buf).cast(),
            8,
            0,
        )
        .user_data(READ);
        self.ring.push(&sqe).expect("room for the read");
    }

    /// Wakes the peer at `ring_fd` / `efd`. The folded way only queues the
    /// message: the enter of this end's next wait submits it.
    fn wake(&mut self, via: Via, ring_fd: RawFd, efd: RawFd) {
        match via {
            Via::Eventfd => {
                let one = 1u64;
                // SAFETY: writes 8 bytes from a live u64.
                let n = unsafe { libc::write(efd, (&one as *const u64).cast(), 8) };
                assert_eq!(n, 8);
            }
            Via::MsgNow | Via::MsgFolded => {
                let sqe = Sqe::msg_ring(ring_fd, WAKE, 0)
                    .flags(IOSQE_CQE_SKIP_SUCCESS)
                    .user_data(0);
                self.ring.push(&sqe).expect("room for the message");
                if via == Via::MsgNow {
                    self.ring.submit().expect("submit");
                }
            }
        }
    }

    /// Waits for this end's wake.
    fn wait(&mut self, via: Via) {
        loop {
            match self.ring.submit_and_wait(1, Some(Duration::from_secs(10))) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("wait: {e}"),
            }
            let mut woken = false;
            for cqe in self.ring.completions() {
                assert!(cqe.res >= 0, "a completion failed: {cqe:?}");
                woken |= matches!(cqe.user_data, WAKE | READ);
            }
            if woken {
                if via == Via::Eventfd {
                    self.arm();
                }
                return;
            }
        }
    }
}

/// CPU time this thread used so far.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes the timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// One run of `rounds` round trips: wall time and both threads' CPU time
/// per round trip, in nanoseconds; `None` without io_uring.
fn run(via: Via, rounds: u64) -> Option<(f64, f64)> {
    let (tx, rx) = mpsc::channel::<Option<(RawFd, RawFd)>>();
    let (back_tx, back_rx) = mpsc::channel::<(RawFd, RawFd)>();
    let ponger = thread::spawn(move || {
        let Some(mut me) = End::new() else {
            tx.send(None).unwrap();
            return None;
        };
        if via == Via::Eventfd {
            me.arm();
        }
        tx.send(Some((me.ring.as_raw_fd(), me.efd.as_raw_fd())))
            .unwrap();
        let (peer_ring, peer_efd) = back_rx.recv().unwrap();
        let cpu = thread_cpu();
        for _ in 0..rounds {
            me.wait(via);
            me.wake(via, peer_ring, peer_efd);
        }
        // The last wake of the folded way goes with no wait after it.
        me.ring.submit().unwrap();
        Some(thread_cpu() - cpu)
    });
    let (peer_ring, peer_efd) = rx.recv().unwrap()?;
    let mut me = End::new()?;
    if via == Via::Eventfd {
        me.arm();
    }
    back_tx
        .send((me.ring.as_raw_fd(), me.efd.as_raw_fd()))
        .unwrap();
    let (start, cpu) = (Instant::now(), thread_cpu());
    for _ in 0..rounds {
        me.wake(via, peer_ring, peer_efd);
        me.wait(via);
    }
    let wall = start.elapsed();
    let cpu = thread_cpu() - cpu + ponger.join().unwrap()?;
    let per = |d: Duration| d.as_nanos() as f64 / rounds as f64;
    Some((per(wall), per(cpu)))
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Every way wakes the parked ring each round: none is lost.
#[test]
fn every_way_of_waking_a_ring_wakes_it() {
    for via in [Via::Eventfd, Via::MsgNow, Via::MsgFolded] {
        if run(via, 200).is_none() {
            return;
        }
    }
}

#[test]
#[ignore = "measurement: run with --release -- --ignored --nocapture bench_cross_ring_wake"]
fn bench_cross_ring_wake() {
    const ROUNDS: u64 = 50_000;
    const RUNS: usize = 7;
    for via in [Via::Eventfd, Via::MsgNow, Via::MsgFolded] {
        let (mut wall, mut cpu) = (Vec::new(), Vec::new());
        for _ in 0..RUNS {
            let Some((w, c)) = run(via, ROUNDS) else {
                return;
            };
            wall.push(w);
            cpu.push(c);
        }
        println!(
            "wake via={via:?} rounds={ROUNDS} runs={RUNS} median_ns_per_round_trip={:.0} \
             median_cpu_ns_per_round_trip={:.0}",
            median(wall),
            median(cpu)
        );
    }
}
