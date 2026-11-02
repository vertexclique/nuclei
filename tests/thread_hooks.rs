//! A thread hook or name function that panics loses nothing the runtime
//! holds: the start hook and the name of a runner the watch starts for a
//! blocked worker's shard, the park and unpark hooks of a worker, the start
//! hook and the name of a blocking-pool thread. Each panic is caught where the
//! hook is called, once the panic hook reported it, and the thread goes on
//! (proofs/tla/hooks, proofs/tla/pool/Start.tla); a name that panics as a
//! worker starts reaches the caller before anything is claimed.
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

/// The watch takes the shard of a worker blocked in one poll and names the
/// runner it starts for it with a name function that panics: the run handed
/// to the thread to be went with the panic, the shard and the task queued on
/// it behind the blocked one with it, and the watch's own thread too. The
/// panic is caught, the runner takes the default name, and the task runs.
#[test]
fn a_name_that_panics_on_a_runner_the_watch_starts_loses_no_task_of_the_shard() {
    let names = Arc::new(AtomicUsize::new(0));
    let named = Arc::clone(&names);
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .thread_name_fn(move || {
            // The first worker is named as any; the runner the watch starts is not.
            if named.fetch_add(1, Ordering::SeqCst) >= 1 {
                panic!("a name that panics");
            }
            "named-worker".to_string()
        })
        .build()
        .expect("the runtime starts");
    let open = Arc::new(AtomicBool::new(false));
    let gate = Arc::clone(&open);
    let (behind_tx, behind_rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        // On this worker's shard, behind the poll that blocks below.
        behind_tx
            .send(nuclei::spawn(async {
                std::thread::current().name().map(str::to_string)
            }))
            .unwrap();
        eventually(|| gate.load(Ordering::SeqCst));
    })
    .detach();
    let behind = behind_rx
        .recv_timeout(SAFETY)
        .expect("the blocking task never ran");
    let out = rt.block_on(nuclei::time::timeout(SAFETY, behind.join()));
    open.store(true, Ordering::SeqCst);
    assert!(
        names.load(Ordering::SeqCst) >= 2,
        "the watch never named a runner"
    );
    match out {
        Ok(Ok(name)) => assert_eq!(
            name.as_deref(),
            Some("nuclei-worker"),
            "the runner did not take the default name"
        ),
        other => panic!("the shard handed to a runner whose name panicked was lost: {other:?}"),
    }
}

/// A blocking-pool thread whose name function panics: the panic left
/// `spawn_blocking` with the thread counted and never started, so a pool of
/// one thread never ran a job again. The panic is caught, the thread takes the
/// default name, and every job runs.
#[test]
fn a_name_that_panics_on_a_blocking_thread_loses_no_job() {
    let armed = Arc::new(AtomicBool::new(false));
    let arm = Arc::clone(&armed);
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(None)
        .max_blocking_threads(1)
        .thread_name_fn(move || {
            if arm.swap(false, Ordering::SeqCst) {
                panic!("a name that panics");
            }
            "named".to_string()
        })
        .build()
        .expect("the runtime starts");
    armed.store(true, Ordering::SeqCst);
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.spawn_blocking(|| std::thread::current().name().map(str::to_string))
    }));
    assert!(!armed.load(Ordering::SeqCst), "the name never panicked");
    let second = rt.spawn_blocking(|| 7u32);
    let out = rt.block_on(nuclei::time::timeout(SAFETY, second.join()));
    assert!(
        matches!(out, Ok(Ok(7))),
        "a job after the name's panic never ran"
    );
    let first = first.expect("the name's panic left spawn_blocking");
    let out = rt.block_on(nuclei::time::timeout(SAFETY, first.join()));
    match out {
        Ok(Ok(name)) => assert_eq!(
            name.as_deref(),
            Some("nuclei-blocking"),
            "the pool thread did not take the default name"
        ),
        other => panic!("the job whose thread's name panicked was lost: {other:?}"),
    }
}

/// A worker start whose name function panics: the start had claimed the
/// worker's place and made its shard before it named the thread, so the place
/// stayed claimed and empty, and no later start could fill it. The thread is
/// named first: the panic reaches the caller with nothing claimed, and a later
/// start takes the place.
#[test]
fn a_name_that_panics_as_a_worker_starts_reaches_the_caller_and_loses_no_place() {
    let armed = Arc::new(AtomicBool::new(false));
    let arm = Arc::clone(&armed);
    let rt = Builder::new()
        .worker_threads(1)
        .max_worker_threads(2)
        .pin_workers(false)
        .handoff_after(None)
        .thread_name_fn(move || {
            if arm.swap(false, Ordering::SeqCst) {
                panic!("a name that panics");
            }
            "named".to_string()
        })
        .build()
        .expect("the runtime starts");
    armed.store(true, Ordering::SeqCst);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(nuclei::spawn_more_threads(1))
    }));
    assert!(!armed.load(Ordering::SeqCst), "the name never panicked");
    assert!(caught.is_err(), "the name's panic never reached the caller");
    let started = rt
        .block_on(nuclei::spawn_more_threads(1))
        .expect("the worker starts");
    assert_eq!(started, 1, "the place the panicking start claimed was lost");
    let task = rt
        .handle()
        .spawn_on_worker(1, async { 5u32 })
        .expect("the second worker is there");
    let out = rt.block_on(nuclei::time::timeout(SAFETY, task.join()));
    assert!(
        matches!(out, Ok(Ok(5))),
        "the second worker never ran a task"
    );
}

/// A name with a NUL byte, which no system's thread name holds, made every
/// thread's spawn panic, where nothing caught it. Each thread takes the name
/// up to the NUL byte.
#[test]
fn a_name_with_a_nul_byte_names_each_thread_up_to_it() {
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(None)
        .thread_name("named\0rest")
        .build()
        .expect("the runtime starts");
    let job = rt.spawn_blocking(|| std::thread::current().name().map(str::to_string));
    let task = rt.spawn(async { std::thread::current().name().map(str::to_string) });
    let (job, task) = rt.block_on(async {
        (
            nuclei::time::timeout(SAFETY, job.join()).await,
            nuclei::time::timeout(SAFETY, task.join()).await,
        )
    });
    assert!(
        matches!(job, Ok(Ok(Some(ref name))) if name == "named"),
        "the pool thread's name: {job:?}"
    );
    assert!(
        matches!(task, Ok(Ok(Some(ref name))) if name == "named"),
        "the worker's name: {task:?}"
    );
}
