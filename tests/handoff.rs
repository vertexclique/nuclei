//! The shard handoff through the public surface: a task blocked in one poll
//! (a blocking call, a loop with no `.await`) holds up only itself, its
//! shard's other tasks, timers and IO going on on another thread; its own
//! wake, a nested `block_on` and the tasks `spawn_local` keeps on its thread
//! finish there; a storm of blocked tasks keeps the threads bounded; a
//! shutdown does not wait for a thread still blocked; and with the handoff
//! off nothing of it runs.
//!
//! Each runtime has one worker, so every task spawned on it shares the
//! blocked task's shard. The bounds are loose: the shard moves within the
//! threshold and a quarter (12.5 ms at 10 ms), and what is checked is that
//! the shard's work does not wait out the 300 ms block.

use std::cell::RefCell;
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use nuclei::runtime::{Builder, Runtime};
use nuclei::sync::mpsc;

/// How long the blocked task blocks.
const BLOCK: Duration = Duration::from_millis(300);
/// What the shard's other work may take with the handoff on: well under the
/// block, with room for a loaded machine.
const WITHIN: Duration = Duration::from_millis(150);

fn runtime(handoff: Option<Duration>) -> Runtime {
    Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(handoff)
        .build()
        .expect("the runtime starts")
}

fn on() -> Runtime {
    runtime(Some(Duration::from_millis(10)))
}

/// Spawns, on `rt`'s one worker, a task that blocks the thread for `BLOCK`
/// in its first poll, then gives `out`; waits until its poll began.
fn blocker(rt: &Runtime, how: fn(), out: u32) -> nuclei::Task<(u32, ThreadId)> {
    let started = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&started);
    let task = rt.spawn(async move {
        seen.store(true, Ordering::SeqCst);
        how();
        (out, std::thread::current().id())
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the blocking task never ran");
        std::thread::yield_now();
    }
    task
}

fn sleep_in_poll() {
    std::thread::sleep(BLOCK);
}

fn spin_in_poll() {
    let until = Instant::now() + BLOCK;
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

#[test]
fn a_task_asleep_in_its_poll_holds_up_no_timer_channel_or_socket_of_its_shard() {
    let rt = on();
    let blocked = blocker(&rt, sleep_in_poll, 7);
    let start = Instant::now();
    // A timer.
    let timer = rt.spawn(async move {
        nuclei::time::sleep(Duration::from_millis(20)).await;
        start.elapsed()
    });
    // A ping-pong over two channels between two tasks.
    let (to_b, mut at_b) = mpsc::channel::<u32>(1);
    let (to_a, mut at_a) = mpsc::channel::<u32>(1);
    let pong = rt.spawn(async move {
        while let Some(n) = at_b.recv().await {
            if to_a.send(n + 1).await.is_err() {
                break;
            }
        }
    });
    let ping = rt.spawn(async move {
        let mut n = 0;
        for _ in 0..100 {
            to_b.send(n).await.unwrap();
            n = at_a.recv().await.unwrap();
        }
        (n, start.elapsed())
    });
    // A TCP echo on the shard.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let echo = rt.spawn(async move {
        let stream = nuclei::Handle::<TcpStream>::connect(addr).await.unwrap();
        stream.send(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        let mut got = 0;
        while got < buf.len() {
            got += stream.recv(&mut buf[got..]).await.unwrap();
        }
        (buf, start.elapsed())
    });
    let (mut peer, _) = listener.accept().unwrap();
    std::io::copy(
        &mut std::io::Read::take(&mut peer.try_clone().unwrap(), 5),
        &mut peer,
    )
    .unwrap();
    let fired = rt.block_on(timer);
    let (n, pinged) = rt.block_on(ping);
    let (bytes, echoed) = rt.block_on(echo);
    assert!(fired < WITHIN, "the timer waited out the block: {fired:?}");
    assert_eq!(n, 100);
    assert!(
        pinged < WITHIN,
        "the ping-pong waited out the block: {pinged:?}"
    );
    assert_eq!(&bytes, b"hello");
    assert!(echoed < WITHIN, "the echo waited out the block: {echoed:?}");
    assert_eq!(
        rt.block_on(blocked).0,
        7,
        "the blocked task's output reaches its handle"
    );
    rt.block_on(pong);
}

#[test]
fn a_task_spinning_in_its_poll_holds_up_no_timer_of_its_shard() {
    let rt = on();
    let blocked = blocker(&rt, spin_in_poll, 9);
    let start = Instant::now();
    let fired = rt.block_on(rt.spawn(async move {
        nuclei::time::sleep(Duration::from_millis(20)).await;
        start.elapsed()
    }));
    assert!(fired < WITHIN, "the timer waited out the spin: {fired:?}");
    assert_eq!(rt.block_on(blocked).0, 9);
}

#[test]
fn a_blocked_task_woken_in_its_poll_runs_again_on_the_shards_new_thread() {
    let rt = on();
    let task = rt.spawn(async {
        let first = std::thread::current().id();
        let mut polls = 0;
        core::future::poll_fn(|cx| {
            polls += 1;
            if polls == 1 {
                cx.waker().wake_by_ref();
                std::thread::sleep(BLOCK);
                return core::task::Poll::Pending;
            }
            core::task::Poll::Ready(())
        })
        .await;
        (first, std::thread::current().id())
    });
    let (first, then) = rt.block_on(task);
    assert_ne!(first, then, "the second poll ran where the shard went");
}

#[test]
fn a_nested_block_on_whose_shard_goes_finishes_on_the_blocked_thread() {
    let rt = on();
    let task = rt.spawn(async {
        let outer = std::thread::current().id();
        let (value, inner) = nuclei::block_on(async {
            nuclei::block_on(async {
                nuclei::block_on(async {
                    std::thread::sleep(BLOCK);
                    // Pending after the shard went: the rest runs on the
                    // thread's own shard.
                    nuclei::time::sleep(Duration::from_millis(5)).await;
                    (5, std::thread::current().id())
                })
            })
        });
        (value, outer, inner)
    });
    let start = Instant::now();
    let timer = rt.block_on(rt.spawn(async move {
        nuclei::time::sleep(Duration::from_millis(20)).await;
        start.elapsed()
    }));
    assert!(
        timer < WITHIN,
        "the shard waited out the nested block: {timer:?}"
    );
    let (value, outer, inner) = rt.block_on(task);
    assert_eq!(value, 5);
    assert_eq!(outer, inner, "the nested future finished on its own thread");
}

#[test]
fn local_tasks_of_a_blocked_thread_finish_on_it_never_on_the_new_runner() {
    let rt = on();
    let task = rt.spawn(async {
        let mine = std::thread::current().id();
        let local = nuclei::spawn_local(async move {
            // Not `Send`: the task may run on its thread only.
            let seen = Rc::new(RefCell::new(Vec::new()));
            for _ in 0..5 {
                seen.borrow_mut().push(std::thread::current().id());
                nuclei::time::sleep(Duration::from_millis(30)).await;
            }
            seen.take()
        });
        // Let the local task run once, then block the thread.
        nuclei::time::sleep(Duration::from_millis(1)).await;
        std::thread::sleep(BLOCK);
        let threads = local.await;
        (mine, threads)
    });
    let start = Instant::now();
    let timer = rt.block_on(rt.spawn(async move {
        nuclei::time::sleep(Duration::from_millis(20)).await;
        start.elapsed()
    }));
    assert!(timer < WITHIN, "the shard waited out the block: {timer:?}");
    let (mine, threads) = rt.block_on(task);
    assert_eq!(threads.len(), 5);
    assert!(
        threads.iter().all(|&t| t == mine),
        "a local task ran off its thread: {threads:?}, its own {mine:?}"
    );
}

#[test]
fn a_receive_in_flight_across_a_handoff_gets_its_bytes_in_order() {
    let rt = on();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let receiving = Arc::new(AtomicBool::new(false));
    let up = Arc::clone(&receiving);
    let reader = rt.spawn(async move {
        let stream = nuclei::Handle::<TcpStream>::connect(addr).await.unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        up.store(true, Ordering::SeqCst);
        while got.len() < 8 {
            let n = stream.recv(&mut buf).await.unwrap();
            assert!(n > 0, "the peer closed early");
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    let (mut peer, _) = listener.accept().unwrap();
    while !receiving.load(Ordering::SeqCst) {
        std::thread::yield_now();
    }
    // The receive is in flight on the shard; its thread blocks now.
    std::thread::sleep(Duration::from_millis(20));
    let blocked = blocker(&rt, sleep_in_poll, 1);
    std::thread::sleep(Duration::from_millis(40));
    std::io::Write::write_all(&mut peer, b"abcd").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    std::io::Write::write_all(&mut peer, b"efgh").unwrap();
    let start = Instant::now();
    let got = rt.block_on(reader);
    assert_eq!(got, b"abcdefgh");
    assert!(start.elapsed() < WITHIN, "the receive waited out the block");
    rt.block_on(blocked);
}

#[cfg(target_os = "linux")]
fn worker_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .filter(|name| name.trim() == "nuclei-handoffs")
        .count()
}

#[cfg(target_os = "linux")]
#[test]
fn a_storm_of_tasks_blocking_in_turn_keeps_the_threads_bounded() {
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .thread_name("nuclei-handoffs")
        .handoff_after(Some(Duration::from_millis(5)))
        .build()
        .unwrap();
    let mut most = 0;
    for round in 0..12 {
        let task = rt.spawn(async move {
            std::thread::sleep(Duration::from_millis(40));
            round
        });
        assert_eq!(rt.block_on(task), round);
        most = most.max(worker_threads());
    }
    // One blocked at a time: the worker's first thread, the one blocked and
    // the one it handed to, reused in turn.
    assert!(most <= 3, "{most} threads for one worker");
}

#[test]
fn a_shutdown_does_not_wait_for_a_thread_still_blocked() {
    let rt = on();
    let _blocked = blocker(&rt, || std::thread::sleep(Duration::from_secs(3)), 0);
    // The shard goes to another thread first: a task of it runs only then.
    rt.block_on(rt.spawn(async {}));
    let start = Instant::now();
    drop(rt);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "the shutdown waited for the blocked thread: {:?}",
        start.elapsed()
    );
}

#[test]
fn with_the_handoff_off_a_blocked_task_holds_up_its_shard_as_before() {
    let rt = runtime(None);
    let blocked = blocker(&rt, sleep_in_poll, 3);
    let start = Instant::now();
    let fired = rt.block_on(rt.spawn(async move {
        nuclei::time::sleep(Duration::from_millis(1)).await;
        start.elapsed()
    }));
    assert!(
        fired >= BLOCK - Duration::from_millis(20),
        "the timer ran during the block: {fired:?}"
    );
    assert_eq!(rt.block_on(blocked).0, 3);
}

#[test]
fn the_stats_count_the_handoff_and_the_long_poll() {
    let rt = on();
    let blocked = blocker(&rt, sleep_in_poll, 0);
    rt.block_on(blocked);
    // This runtime's shard is alive, and others may be too: the stats list
    // every shard of the process, and other tests' come and go.
    assert!(
        nuclei::stats()
            .shards
            .iter()
            .any(|s| s.handoffs >= 1 && s.longest_poll >= Duration::from_millis(10)),
        "no shard counted the handoff and its long poll"
    );
}
