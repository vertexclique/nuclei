//! The runtime's API around its handle: entering a runtime without
//! `block_on`, the current handle without the default's fallback, runtime
//! ids, worker indices, a shutdown that does not wait, thread hooks and the
//! scheduler knobs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nuclei::runtime::{Runtime, RuntimeHandle, worker_index};

fn runtime(workers: usize, name: &'static str) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .thread_name(name)
        .build()
        .expect("the runtime starts")
}

#[test]
fn an_entered_runtime_is_the_one_spawn_uses() {
    let rt = runtime(1, "entered-worker");
    let guard = rt.enter();
    let task = nuclei::spawn(async { std::thread::current().name().map(str::to_owned) });
    drop(guard);
    assert_eq!(rt.block_on(task).as_deref(), Some("entered-worker"));
}

#[test]
fn try_current_answers_only_inside_a_runtime() {
    let err = RuntimeHandle::try_current().unwrap_err();
    assert!(err.is_missing_context());
    assert!(err.to_string().contains("no runtime is set"));
    let rt = runtime(1, "current-worker");
    let id = rt.id();
    rt.block_on(async move {
        assert_eq!(RuntimeHandle::try_current().unwrap().id(), id);
        let on_worker = nuclei::spawn(async { RuntimeHandle::try_current().map(|h| h.id()) });
        assert_eq!(on_worker.await.unwrap(), id);
    });
    let _guard = rt.enter();
    assert_eq!(RuntimeHandle::try_current().unwrap().id(), id);
}

#[test]
fn runtimes_have_distinct_ids_their_handles_share() {
    let (a, b) = (runtime(1, "id-a"), runtime(1, "id-b"));
    assert_ne!(a.id(), b.id());
    assert_eq!(a.handle().id(), a.id());
    assert_eq!(format!("{}", a.id()), format!("{}", a.handle().id()));
}

#[test]
fn worker_index_names_the_worker_and_nothing_else() {
    let rt = runtime(3, "index-worker");
    rt.block_on(async {
        assert_eq!(
            worker_index(),
            None,
            "block_on's own thread is not a worker"
        );
        let mut seen = Vec::new();
        for _ in 0..12 {
            let index = nuclei::spawn(async { worker_index() }).await;
            let index = index.expect("tasks run on workers");
            assert!(index < 3);
            seen.push(index);
        }
        assert_eq!(nuclei::spawn_blocking(worker_index).await, None);
    });
    assert_eq!(worker_index(), None);
}

#[test]
fn shutdown_background_does_not_wait_for_a_busy_worker() {
    let rt = runtime(1, "background-worker");
    let started = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&started);
    rt.spawn(async move {
        flag.store(1, Ordering::SeqCst);
        // A task that holds its worker for a long while.
        std::thread::sleep(Duration::from_secs(3));
    })
    .detach();
    let deadline = Instant::now() + Duration::from_secs(10);
    while started.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let before = Instant::now();
    rt.shutdown_background();
    assert!(
        before.elapsed() < Duration::from_secs(1),
        "it waited for the worker"
    );
}

#[test]
fn thread_hooks_run_on_every_thread_the_runtime_starts() {
    let (starts, stops, parks, unparks) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    let (s, t, p, u) = (
        Arc::clone(&starts),
        Arc::clone(&stops),
        Arc::clone(&parks),
        Arc::clone(&unparks),
    );
    let rt = Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .on_thread_start(move || {
            s.fetch_add(1, Ordering::SeqCst);
        })
        .on_thread_stop(move || {
            t.fetch_add(1, Ordering::SeqCst);
        })
        .on_thread_park(move || {
            p.fetch_add(1, Ordering::SeqCst);
        })
        .on_thread_unpark(move || {
            u.fetch_add(1, Ordering::SeqCst);
        })
        .build()
        .unwrap();
    rt.block_on(async {
        // A blocking thread starts too.
        nuclei::spawn_blocking(|| ()).await;
        for _ in 0..4 {
            nuclei::spawn(async {}).await;
            nuclei::time::sleep(Duration::from_millis(5)).await;
        }
    });
    assert!(
        starts.load(Ordering::SeqCst) >= 3,
        "two workers and a blocking thread"
    );
    assert!(parks.load(Ordering::SeqCst) >= 1, "an idle worker parks");
    assert!(unparks.load(Ordering::SeqCst) >= 1);
    drop(rt);
    // The workers stopped with the runtime; the blocking thread may still be
    // idling out its keep-alive, so at least the workers are counted.
    assert!(stops.load(Ordering::SeqCst) >= 2);
}

#[test]
fn the_scheduler_knobs_keep_the_runtime_working() {
    let rt = Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .event_interval(1)
        .global_queue_interval(1)
        .max_io_events_per_tick(1)
        .build()
        .unwrap();
    let sum = rt.block_on(async {
        let tasks: Vec<_> = (0..100u64)
            .map(|i| nuclei::spawn(async move { i }))
            .collect();
        let mut sum = 0;
        for task in tasks {
            sum += task.await;
        }
        nuclei::time::sleep(Duration::from_millis(2)).await;
        sum
    });
    assert_eq!(sum, 4950);
}

#[test]
#[should_panic(expected = "event_interval must be at least 1")]
fn a_zero_event_interval_is_refused() {
    let _ = Runtime::builder().event_interval(0);
}

#[test]
fn task_hooks_see_every_spawn_poll_and_end() {
    use std::collections::HashSet;
    use std::sync::Mutex;
    let spawned = Arc::new(Mutex::new(HashSet::new()));
    let ended = Arc::new(Mutex::new(HashSet::new()));
    let (before, after) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (s, e, b, a) = (
        Arc::clone(&spawned),
        Arc::clone(&ended),
        Arc::clone(&before),
        Arc::clone(&after),
    );
    let rt = Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .on_task_spawn(move |meta| {
            assert!(s.lock().unwrap().insert(meta.id()), "a task spawned twice");
        })
        .on_before_task_poll(move |_| {
            b.fetch_add(1, Ordering::SeqCst);
        })
        .on_after_task_poll(move |_| {
            a.fetch_add(1, Ordering::SeqCst);
        })
        .on_task_terminate(move |meta| {
            assert!(e.lock().unwrap().insert(meta.id()), "a task ended twice");
        })
        .build()
        .unwrap();
    rt.block_on(async {
        let mut ids = Vec::new();
        for _ in 0..10 {
            let task = nuclei::spawn(async {
                nuclei::task::yield_now().await;
                nuclei::task::yield_now().await;
            });
            ids.push(task.id());
            task.await;
        }
        let local = nuclei::spawn_local(async {});
        ids.push(local.id());
        local.await;
        // A cancelled task ends too.
        let pending = nuclei::spawn(std::future::pending::<()>());
        ids.push(pending.id());
        pending.abort();
        let _ = pending.join().await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while ended.lock().unwrap().len() < ids.len() && Instant::now() < deadline {
            nuclei::task::yield_now().await;
        }
        let spawned = spawned.lock().unwrap().clone();
        let ended = ended.lock().unwrap().clone();
        for id in &ids {
            assert!(spawned.contains(id), "no spawn hook for {id}");
            assert!(ended.contains(id), "no terminate hook for {id}");
        }
    });
    let (b, a) = (before.load(Ordering::SeqCst), after.load(Ordering::SeqCst));
    assert_eq!(b, a, "every poll has both hooks");
    assert!(b >= 10 * 3, "each spawned task was polled three times: {b}");
}

#[test]
fn a_runtime_without_task_hooks_calls_none() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    // A runtime with a hook, so the process-wide flag is up.
    let _hooked = Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .on_task_spawn(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .build()
        .unwrap();
    let plain = runtime(1, "plain-worker");
    plain.block_on(async {
        nuclei::spawn(async {}).await;
        nuclei::spawn_local(async {}).await;
    });
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "another runtime's hook ran"
    );
}
