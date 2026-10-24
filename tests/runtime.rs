//! The runtime through its public surface: `block_on`, `spawn`,
//! `spawn_local`, `spawn_blocking`, nesting, single-threaded mode,
//! cancellation, panics, shutdown, and wakes from threads the runtime does
//! not own.

use std::cell::Cell;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use nuclei::runtime::{Runtime, RuntimeHandle, spawn, spawn_local};
use nuclei::sync::oneshot;

fn runtime(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

/// Counts its drops.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Pending until its flag is set; the setter wakes the stored waker.
#[derive(Clone, Default)]
struct Gate(Arc<Mutex<(bool, Option<Waker>)>>);

impl Gate {
    fn open(&self) {
        let waker = {
            let mut g = self.0.lock().unwrap();
            g.0 = true;
            g.1.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl Future for Gate {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut g = self.0.lock().unwrap();
        if g.0 {
            Poll::Ready(())
        } else {
            g.1 = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Waits for `cond` with a deadline and a growing pause, never a fixed sleep.
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut pause = Duration::from_micros(50);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

#[test]
fn block_on_returns_the_output() {
    let rt = runtime(1);
    assert_eq!(rt.block_on(async { 40 + 2 }), 42);
}

#[test]
fn block_on_waits_for_a_wake_from_another_thread() {
    let rt = runtime(1);
    let gate = Gate::default();
    let opener = {
        let gate = gate.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            gate.open();
        })
    };
    rt.block_on(gate);
    opener.join().unwrap();
}

#[test]
fn spawned_tasks_run_on_the_workers_and_return_their_outputs() {
    let rt = runtime(4);
    let total: u64 = rt.block_on(async {
        let tasks: Vec<_> = (0..1000u64).map(|n| spawn(async move { n })).collect();
        let mut sum = 0;
        for t in tasks {
            sum += t.await;
        }
        sum
    });
    assert_eq!(total, 999 * 1000 / 2);
}

#[test]
fn tasks_spread_over_the_workers() {
    let rt = runtime(4);
    let seen = Arc::new(Mutex::new(HashSet::new()));
    rt.block_on(async {
        let tasks: Vec<_> = (0..400)
            .map(|_| {
                let seen = Arc::clone(&seen);
                spawn(async move {
                    seen.lock().unwrap().insert(std::thread::current().id());
                    // Stay alive a little so the load counts differ.
                    std::thread::sleep(Duration::from_micros(50));
                })
            })
            .collect();
        for t in tasks {
            t.await;
        }
    });
    let workers = seen.lock().unwrap().len();
    assert!(workers >= 2, "400 tasks ran on {workers} worker(s)");
}

#[test]
fn a_task_woken_from_a_foreign_thread_runs() {
    let rt = runtime(2);
    let (tx, rx) = oneshot::channel();
    let task = rt.spawn(async move { rx.await.expect("the sender sends") * 2 });
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        tx.send(21).unwrap();
    });
    assert_eq!(rt.block_on(task), 42);
}

#[test]
fn spawn_local_runs_a_future_that_is_not_send() {
    let rt = runtime(1);
    let out = rt.block_on(async {
        let shared = Rc::new(Cell::new(0));
        let tasks: Vec<_> = (0..10)
            .map(|_| {
                let shared = Rc::clone(&shared);
                spawn_local(async move { shared.set(shared.get() + 1) })
            })
            .collect();
        for t in tasks {
            t.await;
        }
        shared.get()
    });
    assert_eq!(out, 10);
}

/// The reproduction of issue 45: with one worker, a task blocks on another
/// task's output. The nested loop must run that other task on the same shard.
#[test]
fn a_nested_block_on_runs_the_other_tasks_of_its_shard() {
    let rt = runtime(1);
    let out = rt.block_on(async {
        spawn(async {
            let inner = spawn(async { 7 });
            nuclei::runtime::block_on(inner) * 6
        })
        .await
    });
    assert_eq!(out, 42);
}

#[test]
fn nesting_deeper_than_the_limit_panics_with_a_message() {
    let rt = Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .max_nesting(4)
        .build()
        .unwrap();
    fn nest(n: usize) -> usize {
        if n == 0 {
            0
        } else {
            nuclei::runtime::block_on(async move { nest(n - 1) + 1 })
        }
    }
    let ok = rt.block_on(rt.spawn(async { nest(3) }));
    assert_eq!(ok, 3);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let deep = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(rt.spawn(async { nest(8) }))
    }));
    std::panic::set_hook(hook);
    let payload = deep.expect_err("the task panicked at the limit");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_default();
    assert!(message.contains("nested 4 deep"), "{message}");
}

#[test]
fn with_no_workers_tasks_run_on_the_block_on_thread() {
    let rt = runtime(0);
    let here = std::thread::current().id();
    let ran_on = rt.block_on(async { spawn(async { std::thread::current().id() }).await });
    assert_eq!(ran_on, here);
}

#[test]
fn dropping_a_task_drops_its_future() {
    let rt = runtime(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let guard = Counted(Arc::clone(&drops));
    let task = rt.spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await
    });
    drop(task);
    eventually("the cancelled future to drop", || {
        drops.load(Ordering::SeqCst) == 1
    });
}

#[test]
fn a_panicking_task_reaches_its_handle_and_the_worker_goes_on() {
    let rt = runtime(1);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(rt.spawn(async { panic!("boom") }))
    }));
    std::panic::set_hook(hook);
    let payload = panicked.expect_err("awaiting the task resumes its panic");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"boom"));
    assert_eq!(rt.block_on(rt.spawn(async { 5 })), 5, "the worker goes on");
}

#[test]
fn dropping_the_runtime_drops_the_tasks_it_still_holds() {
    let drops = Arc::new(AtomicUsize::new(0));
    let rt = runtime(2);
    let started = Arc::new(AtomicUsize::new(0));
    for _ in 0..8 {
        let guard = Counted(Arc::clone(&drops));
        let started = Arc::clone(&started);
        rt.spawn(async move {
            let _guard = guard;
            started.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await
        })
        .detach();
    }
    eventually("the tasks to start", || started.load(Ordering::SeqCst) == 8);
    drop(rt);
    assert_eq!(drops.load(Ordering::SeqCst), 8);
}

#[test]
fn shutdown_timeout_returns_while_a_worker_is_stuck() {
    let rt = runtime(1);
    let stuck = Arc::new(AtomicBool::new(true));
    let s = Arc::clone(&stuck);
    let entered = Arc::new(AtomicBool::new(false));
    let e = Arc::clone(&entered);
    rt.spawn(async move {
        e.store(true, Ordering::SeqCst);
        while s.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    })
    .detach();
    eventually("the task to start", || entered.load(Ordering::SeqCst));
    let start = Instant::now();
    rt.shutdown_timeout(Duration::from_millis(50));
    assert!(start.elapsed() < Duration::from_secs(5));
    stuck.store(false, Ordering::SeqCst);
}

#[test]
fn spawn_blocking_runs_off_the_shards() {
    let rt = runtime(1);
    let worker = rt.block_on(rt.spawn(async { std::thread::current().id() }));
    let blocking = rt.block_on(rt.spawn_blocking(|| std::thread::current().id()));
    assert_ne!(worker, blocking);
}

#[test]
fn more_blocking_jobs_than_threads_all_finish() {
    let rt = Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .max_blocking_threads(3)
        .build()
        .unwrap();
    let tasks: Vec<_> = (0..64)
        .map(|i| {
            rt.spawn_blocking(move || {
                std::thread::sleep(Duration::from_micros(200));
                i
            })
        })
        .collect();
    let sum: i32 = rt.block_on(async {
        let mut sum = 0;
        for t in tasks {
            sum += t.await;
        }
        sum
    });
    assert_eq!(sum, 63 * 64 / 2);
}

/// A ping-pong pair keeps waking each other into the LIFO slot; a third
/// task on the same shard must still make progress.
#[test]
fn a_ping_pong_pair_does_not_starve_a_third_task() {
    let rt = runtime(1);
    rt.block_on(async {
        let stop = Arc::new(AtomicBool::new(false));
        let (a_tx, mut a_rx) = nuclei::sync::mpsc::channel::<u32>(1);
        let (b_tx, mut b_rx) = nuclei::sync::mpsc::channel::<u32>(1);
        let s = Arc::clone(&stop);
        let ping = spawn(async move {
            let mut n = 0;
            while !s.load(Ordering::Relaxed) {
                b_tx.send(n).await.ok();
                n = a_rx.recv().await.unwrap_or(0);
            }
        });
        let s = Arc::clone(&stop);
        let pong = spawn(async move {
            while !s.load(Ordering::Relaxed) {
                match b_rx.recv().await {
                    Some(n) => {
                        if a_tx.send(n + 1).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        });
        let third = spawn(async { 1 });
        assert_eq!(third.await, 1);
        stop.store(true, Ordering::Relaxed);
        drop(ping);
        drop(pong);
    });
}

#[test]
fn many_threads_waking_tasks_on_parked_shards_lose_no_wake() {
    let rt = runtime(4);
    let rounds = if cfg!(miri) { 10 } else { 2000 };
    let done = Arc::new(AtomicUsize::new(0));
    let mut senders = Vec::new();
    for _ in 0..rounds {
        let (tx, rx) = oneshot::channel::<()>();
        let done = Arc::clone(&done);
        rt.spawn(async move {
            rx.await.unwrap();
            done.fetch_add(1, Ordering::SeqCst);
        })
        .detach();
        senders.push(tx);
    }
    let threads: Vec<_> = senders
        .into_iter()
        .enumerate()
        .fold(
            (0..4).map(|_| Vec::new()).collect::<Vec<_>>(),
            |mut acc, (i, tx)| {
                acc[i % 4].push(tx);
                acc
            },
        )
        .into_iter()
        .map(|batch| {
            std::thread::spawn(move || {
                for tx in batch {
                    // Let the shards go back to sleep now and then.
                    std::thread::yield_now();
                    tx.send(()).unwrap();
                }
            })
        })
        .collect();
    threads.into_iter().for_each(|t| t.join().unwrap());
    eventually("every task to be woken", || {
        done.load(Ordering::SeqCst) == rounds
    });
}

#[test]
fn the_current_handle_inside_a_worker_is_its_runtime() {
    let rt = runtime(2);
    let inside = rt.block_on(rt.spawn(async {
        let handle = RuntimeHandle::current();
        handle.block_on(async { 3 })
    }));
    assert_eq!(inside, 3);
}

/// A task spawned on a worker stays there while the worker is not loaded
/// beyond the margin: one that waits for what it spawned keeps it on its
/// own thread.
#[test]
fn a_task_spawned_on_a_worker_stays_on_it() {
    let rt = runtime(4);
    let same = rt.block_on(rt.spawn(async {
        let here = std::thread::current().id();
        let mut same = 0;
        for _ in 0..50 {
            if spawn(async { std::thread::current().id() }).await == here {
                same += 1;
            }
        }
        same
    }));
    assert_eq!(same, 50);
}

/// A worker holding more live tasks than the margin beyond another shard
/// sends what it spawns elsewhere.
#[test]
fn a_loaded_worker_spreads_what_it_spawns() {
    let rt = Runtime::builder()
        .worker_threads(4)
        .pin_workers(false)
        .local_spawn_margin(4)
        .build()
        .unwrap();
    let threads = Arc::new(Mutex::new(HashSet::new()));
    let gates: Vec<Gate> = (0..64).map(|_| Gate::default()).collect();
    let started = Arc::new(AtomicUsize::new(0));
    let t = Arc::clone(&threads);
    let g = gates.clone();
    let s = Arc::clone(&started);
    let spawner = rt.spawn(async move {
        let tasks: Vec<_> = g
            .into_iter()
            .map(|gate| {
                let (t, s) = (Arc::clone(&t), Arc::clone(&s));
                spawn(async move {
                    t.lock().unwrap().insert(std::thread::current().id());
                    s.fetch_add(1, Ordering::SeqCst);
                    gate.await;
                })
            })
            .collect();
        for task in tasks {
            task.await;
        }
    });
    eventually("every task to start", || {
        started.load(Ordering::SeqCst) == 64
    });
    gates.iter().for_each(Gate::open);
    rt.block_on(spawner);
    let n = threads.lock().unwrap().len();
    assert!(
        n >= 3,
        "64 live tasks with a margin of 4 ran on {n} worker(s)"
    );
}
