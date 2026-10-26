//! The runtime's API around its handle: entering a runtime without
//! `block_on`, the current handle without the default's fallback, runtime
//! ids, worker indices, a shutdown that does not wait, thread hooks, the
//! scheduler knobs (the LIFO slot and the seed among them) and a runtime
//! that shuts down when a task panics.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nuclei::runtime::{RngSeed, Runtime, RuntimeHandle, UnhandledPanic, worker_index};

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

/// Without the LIFO slot, tasks a task spawns run in the order it spawned
/// them; with it, the last one runs first.
#[test]
fn without_the_lifo_slot_tasks_run_in_the_order_they_came() {
    fn order(lifo: bool) -> Vec<u8> {
        let mut builder = Runtime::builder().worker_threads(1).pin_workers(false);
        if !lifo {
            builder = builder.disable_lifo_slot();
        }
        let rt = builder.build().unwrap();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (a, b) = (Arc::clone(&log), Arc::clone(&log));
        rt.block_on(rt.spawn(async move {
            let first = nuclei::spawn(async move { a.lock().unwrap().push(1) });
            let second = nuclei::spawn(async move { b.lock().unwrap().push(2) });
            first.await;
            second.await;
        }));
        log.lock().unwrap().clone()
    }
    assert_eq!(order(true), [2, 1], "the task woken last runs next");
    assert_eq!(order(false), [1, 2]);
}

/// Which branch of 64 fair `select!`s runs, in `block_on` and on a worker.
fn picks(seed: Option<RngSeed>) -> Vec<u8> {
    async fn sixty_four() -> Vec<u8> {
        let mut picks = Vec::new();
        for _ in 0..64 {
            picks.push(nuclei::select! {
                n = core::future::ready(0u8) => n,
                n = core::future::ready(1u8) => n,
                n = core::future::ready(2u8) => n,
                n = core::future::ready(3u8) => n,
            });
        }
        picks
    }
    let mut builder = Runtime::builder().worker_threads(1).pin_workers(false);
    if let Some(seed) = seed {
        builder = builder.rng_seed(seed);
    }
    let rt = builder.build().unwrap();
    let mut picks = rt.block_on(sixty_four());
    picks.extend(rt.block_on(rt.spawn(sixty_four())));
    picks
}

#[test]
fn a_seeded_runtime_repeats_its_random_choices() {
    let seed = RngSeed::from_bytes(b"repeat");
    assert_eq!(picks(Some(seed)), picks(Some(seed)));
    assert_ne!(
        picks(Some(seed)),
        picks(Some(RngSeed::from_bytes(b"other")))
    );
    // Unseeded, the choices differ run to run (all 128 alike: 4^-127).
    assert_ne!(picks(None), picks(None));
}

/// A future's outcome on this thread, outside any runtime, within thirty
/// seconds.
fn wait_here<F: core::future::Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "nothing came within thirty seconds"
        );
        std::thread::park_timeout(Duration::from_millis(100));
    }
}

fn message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// Built to shut down when a task panics, the runtime does: the `block_on`
/// running panics, and every later one; the task's handle still reports
/// its panic; the workers stop and drop the tasks they hold; a task in a
/// nested `block_on` on a worker panics out of it too.
#[test]
fn a_task_panic_shuts_down_a_runtime_built_to_stop_on_one() {
    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let rt = Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .unhandled_panic(UnhandledPanic::ShutdownRuntime)
        .build()
        .unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let guard = Dropped(Arc::clone(&dropped));
    rt.spawn(async move {
        let _guard = guard;
        core::future::pending::<()>().await
    })
    .detach();
    // Sleeps, not waits for good: an implementation that does not shut down
    // fails here in bounded time rather than hanging the test.
    let nested = rt.spawn(async { nuclei::block_on(nuclei::time::sleep(Duration::from_secs(20))) });
    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(async {
            tx.send(nuclei::spawn(async { panic!("a task fails") }))
                .unwrap();
            nuclei::time::sleep(Duration::from_secs(20)).await
        })
    }));
    let said = message(&*ran.expect_err("the block_on panics"));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the block_on heard of the panic only when its own future woke it"
    );
    assert!(
        said.contains("shut down because one of its tasks panicked"),
        "{said}"
    );
    let failed = wait_here(rx.recv().unwrap().join()).expect_err("the task panicked");
    assert_eq!(message(&*failed.into_panic()), "a task fails");
    let nested = wait_here(nested.join()).expect_err("the nested block_on panicked");
    assert!(message(&*nested.into_panic()).contains("shut down because"));
    let later = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(async { 1 })));
    assert!(later.is_err(), "every later block_on panics");
    let deadline = Instant::now() + Duration::from_secs(30);
    while dropped.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "the waiting task was not dropped"
        );
        std::thread::yield_now();
    }
}

/// By default a task's panic is its outcome alone: the runtime runs on.
#[test]
fn a_task_panic_leaves_a_runtime_running_by_default() {
    let rt = runtime(1, "panic-ignored");
    let failed = rt.block_on(rt.spawn(async { panic!("a task fails") }).join());
    assert!(failed.unwrap_err().is_panic());
    assert_eq!(rt.block_on(rt.spawn(async { 7 })), 7);
}
