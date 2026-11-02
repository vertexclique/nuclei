//! A thread hook that panics loses nothing the runtime holds: the start hook
//! of a runner the watch starts for a blocked worker's shard, the park and
//! unpark hooks of a worker, the start hook of a blocking-pool thread. Each
//! panic is caught where the hook is called, once the panic hook reported it,
//! and the thread goes on (proofs/tla/hooks, proofs/tla/pool/Start.tla).
//!
//! Every hook panics only once it is armed, or on one thread start chosen
//! by its name and count, so the runtime starts as it would.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nuclei::runtime::{Builder, Runtime};

/// How long a step may take before the test fails: far past what one needs.
const SAFETY: Duration = Duration::from_secs(10);

/// Waits for `done` until [`SAFETY`] passed; whether it came.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + SAFETY;
    let mut pause = Duration::from_micros(50);
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
    true
}

/// A one-worker runtime, the watch off, with `hooks` set.
fn one_worker(hooks: impl FnOnce(Builder) -> Builder) -> Runtime {
    hooks(
        Builder::new()
            .worker_threads(1)
            .pin_workers(false)
            .handoff_after(None),
    )
    .build()
    .expect("the runtime starts")
}

/// Runs, on `rt`'s one worker, a task that sleeps a millisecond at a time,
/// fifty times, its worker parking at each; arms the hook once the task runs,
/// and waits for its output.
fn sleeps_through_parks(rt: &Runtime, armed: &AtomicBool) {
    let (started, began) = std::sync::mpsc::channel();
    let task = rt.spawn(async move {
        started.send(()).unwrap();
        for _ in 0..50 {
            nuclei::time::sleep(Duration::from_millis(1)).await;
        }
        7u32
    });
    began.recv_timeout(SAFETY).expect("the task never ran");
    armed.store(true, Ordering::SeqCst);
    let out = rt.block_on(nuclei::time::timeout(SAFETY, task.join()));
    assert!(
        matches!(out, Ok(Ok(7))),
        "the worker's task was lost to its hook's panic"
    );
    assert!(!armed.load(Ordering::SeqCst), "the hook never panicked");
}

#[test]
fn a_park_hook_that_panics_loses_no_task_of_its_worker() {
    let armed = Arc::new(AtomicBool::new(false));
    let arm = Arc::clone(&armed);
    let rt = one_worker(|b| {
        b.on_thread_park(move || {
            if arm.swap(false, Ordering::SeqCst) {
                panic!("a park hook that panics");
            }
        })
    });
    sleeps_through_parks(&rt, &armed);
}

#[test]
fn an_unpark_hook_that_panics_loses_no_task_of_its_worker() {
    let armed = Arc::new(AtomicBool::new(false));
    let arm = Arc::clone(&armed);
    let rt = one_worker(|b| {
        b.on_thread_unpark(move || {
            if arm.swap(false, Ordering::SeqCst) {
                panic!("an unpark hook that panics");
            }
        })
    });
    sleeps_through_parks(&rt, &armed);
}

/// The watch takes the shard of a worker blocked in one poll and starts a
/// runner for it, whose start hook panics: the shard, and the task queued on
/// it behind the blocked one, run on that runner all the same.
#[test]
fn a_start_hook_that_panics_on_a_runner_the_watch_starts_loses_no_task_of_the_shard() {
    let worker_starts = Arc::new(AtomicUsize::new(0));
    let starts = Arc::clone(&worker_starts);
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .thread_name("hooked-worker")
        .on_thread_start(move || {
            // The first worker starts as any; the runner the watch starts panics.
            if std::thread::current().name() == Some("hooked-worker")
                && starts.fetch_add(1, Ordering::SeqCst) == 1
            {
                panic!("a start hook that panics");
            }
        })
        .build()
        .expect("the runtime starts");
    let open = Arc::new(AtomicBool::new(false));
    let gate = Arc::clone(&open);
    let (behind_tx, behind_rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        // On this worker's shard, behind the poll that blocks below.
        behind_tx.send(nuclei::spawn(async { 9u32 })).unwrap();
        eventually(|| gate.load(Ordering::SeqCst));
    })
    .detach();
    let behind = behind_rx
        .recv_timeout(SAFETY)
        .expect("the blocking task never ran");
    let out = rt.block_on(nuclei::time::timeout(SAFETY, behind.join()));
    open.store(true, Ordering::SeqCst);
    assert!(
        worker_starts.load(Ordering::SeqCst) >= 2,
        "the watch never started a runner"
    );
    assert!(
        matches!(out, Ok(Ok(9))),
        "the shard handed to a runner whose start hook panicked was lost"
    );
}

/// A blocking-pool thread whose start hook panics went with its count still
/// held: with a pool of one thread no other started, and the job never ran.
#[test]
fn a_start_hook_that_panics_on_a_blocking_thread_loses_no_job() {
    let first = Arc::new(AtomicBool::new(true));
    let once = Arc::clone(&first);
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .max_blocking_threads(1)
        .on_thread_start(move || {
            if std::thread::current().name() == Some("nuclei-blocking")
                && once.swap(false, Ordering::SeqCst)
            {
                panic!("a start hook that panics");
            }
        })
        .build()
        .expect("the runtime starts");
    let job = rt.spawn_blocking(|| 7u32);
    let out = rt.block_on(nuclei::time::timeout(SAFETY, job.join()));
    assert!(!first.load(Ordering::SeqCst), "the hook never panicked");
    assert!(
        matches!(out, Ok(Ok(7))),
        "the job was lost to its pool thread's start hook"
    );
}
