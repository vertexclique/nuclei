//! The shard handoff through the public surface: a task blocked in one poll
//! (a blocking call, a loop with no `.await`) holds up only itself, its
//! shard's other tasks, timers and IO going on on another thread; its own
//! wake, a nested `block_on` and the tasks `spawn_local` keeps on its thread
//! finish there; a storm of blocked tasks keeps the threads bounded; a
//! shutdown does not wait for a thread still blocked; and with the handoff
//! off nothing of it runs.
//!
//! Each runtime has one worker, so every task spawned on it shares the
//! blocked task's shard. Nothing is timed against a bound: a blocked task
//! holds its thread at a [`Gate`] until the test has seen the shard's other
//! work done, and only a shard that waited out the block meets the gate's
//! [`SAFETY`] deadline, which fails the test. A slow machine takes longer and
//! still passes.

use std::cell::RefCell;
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use nuclei::runtime::{Builder, Runtime};
use nuclei::sync::mpsc;

/// How long a task blocks where the test does not end the block.
const BLOCK: Duration = Duration::from_millis(300);
/// How long a gate holds at most: far past anything a working handoff needs.
const SAFETY: Duration = Duration::from_secs(10);

/// A block the test ends: a task holds its thread in one poll until the test
/// opens the gate, or gives up after [`SAFETY`].
#[derive(Default)]
struct Gate {
    open: AtomicBool,
    gave_up: AtomicBool,
}

impl Gate {
    /// Holds the calling thread, asleep or with `spin` spinning, until the
    /// gate opens.
    fn hold(&self, spin: bool) {
        let deadline = Instant::now() + SAFETY;
        while !self.open.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                self.gave_up.store(true, Ordering::SeqCst);
                return;
            }
            if spin {
                std::hint::spin_loop();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Opens the gate, once `what` was done while it still held.
    fn release_after(&self, what: &str) {
        assert!(
            !self.gave_up.load(Ordering::SeqCst),
            "{what} waited out the block"
        );
        self.open.store(true, Ordering::SeqCst);
    }
}

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

/// Spawns, on `rt`'s one worker, a task that blocks the thread with `how` in
/// its first poll, then gives `out`; waits until its poll began.
fn blocker(
    rt: &Runtime,
    how: impl FnOnce() + Send + 'static,
    out: u32,
) -> nuclei::Task<(u32, ThreadId)> {
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

/// A gate held by a task of `rt`'s one worker, asleep or with `spin` spinning.
fn gated(rt: &Runtime, spin: bool, out: u32) -> (Arc<Gate>, nuclei::Task<(u32, ThreadId)>) {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    (gate, blocker(rt, move || held.hold(spin), out))
}

#[test]
fn a_task_asleep_in_its_poll_holds_up_no_timer_channel_or_socket_of_its_shard() {
    let rt = on();
    let (gate, blocked) = gated(&rt, false, 7);
    // A timer.
    let timer = rt.spawn(nuclei::time::sleep(Duration::from_millis(20)));
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
        n
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
        buf
    });
    let (mut peer, _) = listener.accept().unwrap();
    std::io::copy(
        &mut std::io::Read::take(&mut peer.try_clone().unwrap(), 5),
        &mut peer,
    )
    .unwrap();
    rt.block_on(timer);
    let n = rt.block_on(ping);
    let bytes = rt.block_on(echo);
    gate.release_after("the timer, the ping-pong or the echo");
    assert_eq!(n, 100);
    assert_eq!(&bytes, b"hello");
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
    let (gate, blocked) = gated(&rt, true, 9);
    rt.block_on(rt.spawn(nuclei::time::sleep(Duration::from_millis(20))));
    gate.release_after("the timer");
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
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let task = rt.spawn(async move {
        let outer = std::thread::current().id();
        let (value, inner) = nuclei::block_on(async {
            nuclei::block_on(async {
                nuclei::block_on(async {
                    held.hold(false);
                    // Pending after the shard went: the rest runs on the
                    // thread's own shard.
                    nuclei::time::sleep(Duration::from_millis(5)).await;
                    (5, std::thread::current().id())
                })
            })
        });
        (value, outer, inner)
    });
    rt.block_on(rt.spawn(nuclei::time::sleep(Duration::from_millis(20))));
    gate.release_after("the shard's timer, across the nested block,");
    let (value, outer, inner) = rt.block_on(task);
    assert_eq!(value, 5);
    assert_eq!(outer, inner, "the nested future finished on its own thread");
}

#[test]
fn local_tasks_of_a_blocked_thread_finish_on_it_never_on_the_new_runner() {
    let rt = on();
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let task = rt.spawn(async move {
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
        held.hold(false);
        let threads = local.await;
        (mine, threads)
    });
    rt.block_on(rt.spawn(nuclei::time::sleep(Duration::from_millis(20))));
    gate.release_after("the shard's timer");
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
    let (gate, blocked) = gated(&rt, false, 1);
    std::thread::sleep(Duration::from_millis(40));
    std::io::Write::write_all(&mut peer, b"abcd").unwrap();
    std::thread::sleep(Duration::from_millis(5));
    std::io::Write::write_all(&mut peer, b"efgh").unwrap();
    let got = rt.block_on(reader);
    gate.release_after("the receive");
    assert_eq!(got, b"abcdefgh");
    rt.block_on(blocked);
}

/// A task that calls into its shard in its poll, on the shard's own paths (a send on a socket
/// the shard owns, a wake of a task of the shard, a spawn), and then blocks is handed on still:
/// each call marks itself and puts the poll's word back as it ends, so the monitor sees one poll
/// past the threshold. The shard's other work runs meanwhile, and the send reached the peer.
#[test]
fn a_task_that_calls_into_its_shard_and_then_blocks_is_handed_on() {
    let rt = on();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let (to_waiter, mut at_waiter) = mpsc::channel::<u32>(1);
    let waiter = rt.spawn(async move { at_waiter.recv().await });
    let (spawned_tx, spawned_rx) = std::sync::mpsc::channel();
    let blocked = rt.spawn(async move {
        let stream = nuclei::Handle::<TcpStream>::connect(addr).await.unwrap();
        // One poll from here on: the calls, then the block.
        stream.send(b"ping").await.unwrap();
        to_waiter.try_send(3).unwrap();
        let spawned = nuclei::spawn(async { 11u32 });
        spawned_tx.send(spawned).unwrap();
        held.hold(false);
        stream
    });
    let (mut peer, _) = listener.accept().unwrap();
    let mut got = [0u8; 4];
    std::io::Read::read_exact(&mut peer, &mut got).unwrap();
    let spawned = spawned_rx.recv().unwrap();
    assert_eq!(
        rt.block_on(spawned),
        11,
        "the task spawned before the block ran"
    );
    assert_eq!(
        rt.block_on(waiter),
        Some(3),
        "the task woken before the block ran"
    );
    rt.block_on(rt.spawn(nuclei::time::sleep(Duration::from_millis(20))));
    gate.release_after("the woken task, the spawned one or the shard's timer");
    assert_eq!(&got, b"ping");
    drop(rt.block_on(blocked));
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
    let (gate, _blocked) = gated(&rt, false, 0);
    // The shard goes to another thread first: a task of it runs only then.
    rt.block_on(rt.spawn(async {}));
    drop(rt);
    gate.release_after("the shutdown");
}

#[test]
fn with_the_handoff_off_a_blocked_task_holds_up_its_shard_as_before() {
    let rt = runtime(None);
    let ended = Arc::new(AtomicBool::new(false));
    let end = Arc::clone(&ended);
    let blocked = blocker(
        &rt,
        move || {
            std::thread::sleep(BLOCK);
            end.store(true, Ordering::SeqCst);
        },
        3,
    );
    let after = rt.block_on(rt.spawn(async move {
        nuclei::time::sleep(Duration::from_millis(1)).await;
        ended.load(Ordering::SeqCst)
    }));
    assert!(after, "the timer ran during the block");
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

#[test]
fn a_poll_blocked_past_the_threshold_hands_its_shard_over_before_it_ends() {
    // The poll stays blocked until the queued task ran, so a slow runner only
    // takes longer; the monitor's tick precision is checked by the bell's own
    // test.
    let rt = on();
    let started = Arc::new(AtomicBool::new(false));
    let ran = Arc::new(AtomicBool::new(false));
    let (up, seen) = (Arc::clone(&started), Arc::clone(&ran));
    let blocked = rt.spawn(async move {
        up.store(true, Ordering::SeqCst);
        // Held until the queued task ran, which only a handoff lets happen.
        let deadline = Instant::now() + SAFETY;
        while !seen.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        seen.load(Ordering::SeqCst)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the blocking task never ran");
        std::thread::yield_now();
    }
    let quick = rt.spawn(async move { ran.store(true, Ordering::SeqCst) });
    assert!(
        rt.block_on(blocked),
        "the queued task waited out the blocked poll"
    );
    rt.block_on(quick);
}

/// Runs `before`, then holds the thread at a gate, then runs `rest`, all in the first poll of a
/// task of `rt`'s worker 0. Returns once a task of worker 0 ran (the shard moved to another
/// thread) and the gate is open, so `rest` is the rest of a poll the monitor took. The channel
/// gives `rest`'s result.
fn after_take<B, R>(
    rt: &Runtime,
    before: impl FnOnce() -> B + Send + 'static,
    rest: impl FnOnce(B) -> R + Send + 'static,
) -> std::sync::mpsc::Receiver<R>
where
    R: Send + 'static,
{
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let started = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&started);
    let (out, answer) = std::sync::mpsc::channel();
    let handle = rt.handle();
    handle
        .spawn_on_worker(0, async move {
            seen.store(true, Ordering::SeqCst);
            let kept = before();
            held.hold(false);
            let _ = out.send(rest(kept));
        })
        .unwrap()
        .detach();
    let deadline = Instant::now() + SAFETY;
    while !started.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the blocking task never ran");
        std::thread::yield_now();
    }
    rt.block_on(handle.spawn_on_worker(0, async {}).unwrap());
    gate.release_after("a task of the worker's shard");
    answer
}

/// What `rest` of an [`after_take`] answered, failing with `what` if it hung.
fn answered<R>(answer: &std::sync::mpsc::Receiver<R>, what: &str) -> R {
    answer
        .recv_timeout(SAFETY)
        .unwrap_or_else(|_| panic!("{what} hung in the rest of a poll the monitor took"))
}

#[test]
fn the_rest_of_a_taken_poll_answers_its_workers_index_and_backend() {
    let rt = on();
    let answer = after_take(
        &rt,
        || (nuclei::runtime::worker_index(), nuclei::Proactor::backend()),
        |before| {
            (
                before,
                (nuclei::runtime::worker_index(), nuclei::Proactor::backend()),
            )
        },
    );
    let (before, after) = answered(&answer, "worker_index or backend");
    assert_eq!(before.0, Some(0), "the poll ran on worker 0");
    assert_eq!(after, before, "the rest of the poll answers what it did");
}

#[test]
fn stop_current_thread_in_the_rest_of_a_taken_poll_stops_that_worker() {
    let rt = Builder::new()
        .worker_threads(1)
        .max_worker_threads(2)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .build()
        .expect("the runtime starts");
    assert_eq!(rt.block_on(nuclei::spawn_more_threads(1)).unwrap(), 1);
    let answer = after_take(
        &rt,
        || (),
        |()| nuclei::block_on(nuclei::stop_current_thread()),
    );
    assert!(
        answered(&answer, "stop_current_thread"),
        "no worker was stopped"
    );
    let refused = rt.handle().spawn_on_worker(0, async {}).err();
    assert_eq!(
        refused.map(|err| err.kind()),
        Some(std::io::ErrorKind::InvalidInput),
        "worker 0 still takes tasks"
    );
}

#[test]
fn blocking_recv_in_the_rest_of_a_taken_poll_runs_a_local_task_woken_from_afar() {
    let rt = on();
    let answer = after_take(
        &rt,
        || (),
        |()| {
            let (tx, mut rx) = mpsc::channel::<u32>(1);
            let (wake, woken) = nuclei::sync::oneshot::channel::<()>();
            nuclei::spawn_local(async move {
                let _ = woken.await;
                tx.send(9).await.unwrap();
            })
            .detach();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                let _ = wake.send(());
            });
            rx.blocking_recv()
        },
    );
    assert_eq!(answered(&answer, "blocking_recv"), Some(9));
}

#[test]
fn blocking_recv_in_the_rest_of_a_taken_poll_runs_a_local_task_spawned_before_the_take() {
    let rt = on();
    let answer = after_take(
        &rt,
        || {
            let (tx, rx) = mpsc::channel::<u32>(1);
            let (wake, woken) = nuclei::sync::oneshot::channel::<()>();
            nuclei::spawn_local(async move {
                let _ = woken.await;
                tx.send(9).await.unwrap();
            })
            .detach();
            (rx, wake)
        },
        |(mut rx, wake)| {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                let _ = wake.send(());
            });
            rx.blocking_recv()
        },
    );
    assert_eq!(answered(&answer, "blocking_recv"), Some(9));
}

#[test]
fn a_nested_block_on_in_the_rest_of_a_taken_poll_gets_the_wake_of_a_local_task() {
    let rt = on();
    let answer = after_take(
        &rt,
        || (),
        |()| {
            let (wake, woken) = nuclei::sync::oneshot::channel::<()>();
            let local = nuclei::spawn_local(async move {
                let _ = woken.await;
                5u32
            });
            // Long enough for the thread's own shard to sleep before the wake.
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let _ = wake.send(());
            });
            nuclei::block_on(local)
        },
    );
    assert_eq!(answered(&answer, "the nested block_on"), 5);
}

#[test]
fn many_blocking_receives_in_the_rest_of_a_taken_poll_outlast_the_poll_budget() {
    let rt = on();
    let answer = after_take(
        &rt,
        || (),
        |()| {
            let (tx, mut rx) = mpsc::channel::<u32>(1);
            for n in 0..300 {
                tx.try_send(n).unwrap();
                assert_eq!(rx.blocking_recv(), Some(n));
            }
            300
        },
    );
    assert_eq!(answered(&answer, "the blocking receives"), 300);
}

/// Counts its drops.
struct Probe(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_thread_whose_shard_went_holds_nothing_while_it_waits_as_a_spare() {
    let rt = on();
    let freed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Kept by the test: an atom dropped in the task would flush there, before the thread waits.
    let atom = Arc::new(kovan::Atom::new(Probe(Arc::clone(&freed))));
    // The task retires the probe into its thread's kovan batch, then blocks the thread in its
    // poll: the shard goes to another thread, and this one waits as a spare once the poll ends.
    let gate = Arc::new(Gate::default());
    let task = {
        let (held, atom) = (Arc::clone(&gate), Arc::clone(&atom));
        let started = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&started);
        let task = rt.spawn(async move {
            atom.store(Probe(Arc::new(std::sync::atomic::AtomicUsize::new(0))));
            seen.store(true, Ordering::SeqCst);
            held.hold(false);
            std::thread::current().id()
        });
        let deadline = Instant::now() + SAFETY;
        while !started.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the blocking task never ran");
            std::thread::yield_now();
        }
        task
    };
    // Runs only on the shard's new thread, the old one being held at the gate.
    let runner = rt.block_on(rt.spawn(async { std::thread::current().id() }));
    gate.release_after("a task of the shard");
    let blocked = rt.block_on(task);
    assert_ne!(blocked, runner, "the shard never went to another thread");
    // Nothing more runs on the old thread: it waits as a spare, and frees what it retired first.
    kovan::flush();
    let deadline = Instant::now() + SAFETY;
    let mut pause = Duration::from_micros(100);
    while freed.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "a thread whose shard went held what it retired while it waited as a spare"
        );
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(10));
    }
}
