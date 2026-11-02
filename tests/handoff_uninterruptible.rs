//! The shard handoff while the old runner sits in a wait no signal ends: the thread blocked in
//! its poll is inside `clone(CLONE_VFORK)`, which waits, killable only, until the child it made
//! leaves. The shard's new thread must still serve what the old one armed before it blocked: a
//! stream receive waiting for its peer, a write waiting for room, a listener's accept, and a wake
//! from another thread (a task spawned onto the shard), as well as a timer.
//!
//! Linux only: the wait is Linux's `CLONE_VFORK`, and the backend it tells apart is io_uring,
//! whose completions of an operation run as work queued to the thread that submitted it, which
//! a killable wait never runs. epoll runs the same tests, and must pass them as any handoff does.
//!
//! Each runtime has one worker, so every task shares the blocked task's shard. Nothing is timed
//! against a bound: the child holds its parent until the test opens the gate, or until
//! [`SAFETY`], which marks the gate given up and fails the test.
#![cfg(target_os = "linux")]

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use nuclei::Handle;
use nuclei::io::AsyncWriteExt;
use nuclei::runtime::{Builder, Runtime};

/// How long the gate holds at most: far past anything a working handoff needs.
const SAFETY: Duration = Duration::from_secs(10);

/// A block the test ends, in a wait no signal interrupts.
#[derive(Default)]
struct Gate {
    open: AtomicBool,
    gave_up: AtomicBool,
}

impl Gate {
    /// Opens the gate, once `what` was done while it still held.
    fn release_after(&self, what: &str) {
        assert!(
            !self.gave_up.load(Ordering::SeqCst),
            "{what} waited out the old runner's uninterruptible wait"
        );
        self.open.store(true, Ordering::SeqCst);
    }
}

/// The child of [`hold`]: it shares its parent's memory and runs on a stack of its own, reads the
/// gate until it opens or [`SAFETY`] passes, and leaves, which lets its parent go. It calls only
/// the clock and a sleep.
extern "C" fn child(arg: *mut libc::c_void) -> libc::c_int {
    // SAFETY: `hold` passes its gate, alive until this child left (the parent waits for that).
    let gate = unsafe { &*arg.cast::<Gate>() };
    let now = || {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: the call writes the struct it is given.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    };
    let deadline = now() + SAFETY;
    while !gate.open.load(Ordering::SeqCst) {
        if now() >= deadline {
            gate.gave_up.store(true, Ordering::SeqCst);
            break;
        }
        let ms = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        // SAFETY: a plain sleep of the struct it is given.
        unsafe { libc::nanosleep(&ms, core::ptr::null_mut()) };
    }
    0
}

/// Holds the calling thread until the gate opens: `clone(CLONE_VM | CLONE_VFORK)` returns in the
/// parent only once the child left, and the parent waits for that killable only, so the work
/// the kernel queues to the thread meanwhile waits too.
fn hold(gate: &Gate) {
    let mut stack = vec![0u8; 256 << 10];
    // The stack grows down, from its 16-byte aligned top.
    let top = (stack.as_mut_ptr() as usize + stack.len()) & !15;
    // SAFETY: the child runs on its own stack, reads only the gate (alive through the call) and
    // calls only the clock and a sleep, and the parent sleeps in the call until it left.
    let pid = unsafe {
        libc::clone(
            child,
            top as *mut libc::c_void,
            libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD,
            core::ptr::from_ref(gate).cast_mut().cast(),
        )
    };
    assert!(pid > 0, "clone: {}", std::io::Error::last_os_error());
    let mut status = 0;
    // SAFETY: reaps our own child, which already left.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    drop(stack);
}

fn runtime() -> Runtime {
    Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .build()
        .expect("the runtime starts")
}

/// Waits, bounded, until `done`.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + SAFETY;
    let mut pause = Duration::from_micros(50);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

/// Blocks the runtime's one worker in [`hold`] from a task of its shard, and waits until the
/// monitor handed the shard to another thread. Gives the gate, the blocked task and the blocked
/// thread's id. The task blocks in the poll a timer's wake makes, not a wake from another thread:
/// whatever the worker armed on its poller to be woken from elsewhere is still armed then.
fn block_and_hand_off(rt: &Runtime) -> (Arc<Gate>, nuclei::Task<()>, libc::pid_t) {
    let handoffs = || rt.stats().shards.iter().map(|s| s.handoffs).sum::<u64>();
    let before = handoffs();
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let entered = Arc::new(AtomicI32::new(0));
    let seen = Arc::clone(&entered);
    let blocked = rt.spawn(async move {
        nuclei::time::sleep(Duration::from_millis(30)).await;
        // SAFETY: a plain syscall.
        seen.store(unsafe { libc::gettid() }, Ordering::SeqCst);
        hold(&held);
    });
    eventually("the blocking task's poll", || {
        entered.load(Ordering::SeqCst) != 0
    });
    eventually("the handoff", || handoffs() > before);
    (gate, blocked, entered.load(Ordering::SeqCst))
}

/// Whether a thread of this process other than `old`, named as the runtime's workers are, sleeps
/// in its poller's wait: the shard's new thread, with nothing to run.
fn new_runner_sleeps(old: libc::pid_t) -> bool {
    let mut waits = vec![
        libc::SYS_io_uring_enter,
        libc::SYS_epoll_pwait,
        libc::SYS_epoll_pwait2,
    ];
    #[cfg(target_arch = "x86_64")]
    waits.push(libc::SYS_epoll_wait);
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .flatten()
        .any(|task| {
            let tid: libc::pid_t = task.file_name().to_string_lossy().parse().unwrap_or(0);
            let name = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
            let call = std::fs::read_to_string(task.path().join("syscall")).unwrap_or_default();
            let number = call
                .split(' ')
                .next()
                .and_then(|n| n.parse::<libc::c_long>().ok());
            tid != old
                && name.trim() == "nuclei-worker"
                && number.is_some_and(|n| waits.contains(&n))
        })
}

#[test]
fn a_receive_the_old_runner_armed_gets_its_bytes_while_it_cannot_be_interrupted() {
    let rt = runtime();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let first = Arc::new(AtomicBool::new(false));
    let got_first = Arc::clone(&first);
    let reader = rt.spawn(async move {
        let stream = Handle::<TcpStream>::connect(addr).await.unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < 5 {
            let n = stream.recv(&mut buf).await.unwrap();
            assert!(n > 0, "the peer closed early");
            got.extend_from_slice(&buf[..n]);
            // The first byte came through what this shard's first thread armed.
            got_first.store(true, Ordering::SeqCst);
        }
        (got, stream)
    });
    let (mut peer, _) = listener.accept().unwrap();
    peer.write_all(b"x").unwrap();
    eventually("the first byte", || first.load(Ordering::SeqCst));
    let (gate, blocked, _) = block_and_hand_off(&rt);
    peer.write_all(b"abcd").unwrap();
    let (got, stream) = rt.block_on(reader);
    gate.release_after("the receive");
    rt.block_on(blocked);
    assert_eq!(got, b"xabcd");
    // The old runner, back, ends what it armed; the stream's operations all ended, so it hands its
    // socket out.
    let (done, retired) = std::sync::mpsc::channel();
    std::thread::spawn(move || done.send(stream.into_inner()).unwrap());
    retired
        .recv_timeout(SAFETY)
        .expect("the stream waited for good for an operation of its old runner's");
    drop(peer);
}

#[test]
fn a_write_waiting_for_room_the_old_runner_armed_goes_on_while_it_cannot_be_interrupted() {
    let rt = runtime();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let full = Arc::new(AtomicBool::new(false));
    let waits = Arc::clone(&full);
    let data: Vec<u8> = (0..8u32 << 20).map(|i| (i % 251) as u8).collect();
    let sent = data.clone();
    let writer = rt.spawn(async move {
        let mut stream = Handle::<TcpStream>::connect(addr).await.unwrap();
        waits.store(true, Ordering::SeqCst);
        stream.write_all(&sent).await.unwrap();
        stream.flush().await.unwrap();
    });
    let (peer, _) = listener.accept().unwrap();
    // The socket fills while nobody reads: the write waits for room on this shard's first thread.
    eventually("the write", || full.load(Ordering::SeqCst));
    std::thread::sleep(Duration::from_millis(50));
    let (gate, blocked, _) = block_and_hand_off(&rt);
    let mut got = Vec::with_capacity(data.len());
    std::io::Read::read_to_end(&mut std::io::Read::take(&peer, data.len() as u64), &mut got)
        .unwrap();
    rt.block_on(writer);
    gate.release_after("the write");
    rt.block_on(blocked);
    assert!(got == data, "the bytes were damaged or misordered");
}

/// Not on io_uring yet: an accept's poll there is an exclusive waiter of its listener, and a
/// cancelled one the old runner has not ended takes every connection's wake, so the new runner
/// cannot arm one in its place, and the accept waits for the old runner
/// (proofs/tla/ring/RingHandoff.tla, `WRH_accept_waits` and the `adopt_accepts` mutation).
#[test]
fn an_accept_the_old_runner_armed_takes_a_connection_while_it_cannot_be_interrupted() {
    let rt = runtime();
    if rt.stats().shards[0].backend == nuclei::IoBackend::IoUring {
        eprintln!("skipping: on io_uring an accept the old runner armed completes on that thread");
        return;
    }
    let listener = rt
        .block_on(Handle::<TcpListener>::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.get_ref().local_addr().unwrap();
    let first = Arc::new(AtomicBool::new(false));
    let got_first = Arc::clone(&first);
    let acceptor = rt.spawn(async move {
        let (_one, _) = listener.accept().await.unwrap();
        // The first connection came through what this shard's first thread armed.
        got_first.store(true, Ordering::SeqCst);
        let (_two, _) = listener.accept().await.unwrap();
    });
    let _one = TcpStream::connect(addr).unwrap();
    eventually("the first accept", || first.load(Ordering::SeqCst));
    let (gate, blocked, _) = block_and_hand_off(&rt);
    let _two = TcpStream::connect(addr).unwrap();
    rt.block_on(acceptor);
    gate.release_after("the accept");
    rt.block_on(blocked);
}

#[test]
fn a_task_spawned_from_another_thread_runs_while_the_old_runner_cannot_be_interrupted() {
    let rt = runtime();
    // The shard's first thread waits on its poller once, with nothing to run.
    rt.block_on(rt.spawn(nuclei::time::sleep(Duration::from_millis(1))));
    let (gate, blocked, old) = block_and_hand_off(&rt);
    // Spawned once the new thread sleeps on its poller: only a wake from here gets it going.
    eventually("the new thread's sleep", || new_runner_sleeps(old));
    assert_eq!(rt.block_on(rt.spawn(async { 7u32 })), 7);
    gate.release_after("the spawned task");
    rt.block_on(blocked);
}

#[test]
fn a_timer_armed_before_the_block_fires_while_the_old_runner_cannot_be_interrupted() {
    let rt = runtime();
    let armed = Arc::new(AtomicBool::new(false));
    let up = Arc::clone(&armed);
    let timer = rt.spawn(async move {
        let sleep = nuclei::time::sleep(Duration::from_millis(100));
        up.store(true, Ordering::SeqCst);
        sleep.await;
    });
    eventually("the timer's first poll", || armed.load(Ordering::SeqCst));
    let (gate, blocked, _) = block_and_hand_off(&rt);
    rt.block_on(timer);
    gate.release_after("the timer");
    rt.block_on(blocked);
}
