//! Placement on a named worker: `RuntimeHandle::{workers, spawn_on_worker,
//! spawn_local_on_worker, on_each_worker}`, workers that stop, a handoff of a
//! worker's shard, and `task::Builder::worker`.

use std::io::{self, ErrorKind};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use nuclei::runtime::{Runtime, worker_index};
use nuclei::task::Builder as TaskBuilder;

const SAFETY: Duration = Duration::from_secs(10);

fn runtime(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

fn on() -> Runtime {
    Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .build()
        .expect("the runtime starts")
}

fn refused<T>(result: io::Result<T>) -> io::Error {
    match result {
        Ok(_) => panic!("the placement was accepted"),
        Err(err) => err,
    }
}

fn invalid<T>(result: io::Result<T>) -> io::Error {
    let err = refused(result);
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
    err
}

struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Gate {
    open: AtomicBool,
    gave_up: AtomicBool,
}

impl Gate {
    fn hold(&self) {
        let deadline = Instant::now() + SAFETY;
        while !self.open.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                self.gave_up.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn release_after(&self, what: &str) {
        assert!(
            !self.gave_up.load(Ordering::SeqCst),
            "{what} waited out the block"
        );
        self.open.store(true, Ordering::SeqCst);
    }
}

fn wait_for(what: &str, flag: &AtomicBool) {
    let deadline = Instant::now() + SAFETY;
    while !flag.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::yield_now();
    }
}

#[test]
fn workers_counts_the_workers_the_runtime_started() {
    assert_eq!(runtime(3).handle().workers(), 3);
    assert_eq!(runtime(1).handle().workers(), 1);
    assert_eq!(runtime(0).handle().workers(), 0);
}

#[test]
fn a_task_placed_on_a_worker_runs_there() {
    let rt = runtime(3);
    let h = rt.handle();
    for _ in 0..4 {
        for k in 0..3 {
            let task = h
                .spawn_on_worker(k, async move { (k, worker_index()) })
                .unwrap();
            assert_eq!(rt.block_on(task), (k, Some(k)));
        }
    }
}

#[test]
fn a_task_placed_on_a_worker_keeps_it_across_awaits() {
    let rt = runtime(3);
    let task = rt
        .handle()
        .spawn_on_worker(2, async {
            let mut seen = Vec::new();
            for _ in 0..20 {
                nuclei::task::yield_now().await;
                nuclei::time::sleep(Duration::from_millis(1)).await;
                seen.push(worker_index());
            }
            seen
        })
        .unwrap();
    assert!(rt.block_on(task).iter().all(|&i| i == Some(2)));
}

#[test]
fn a_worker_the_runtime_does_not_have_refuses_the_task_and_drops_it_unrun() {
    let rt = runtime(2);
    let h = rt.handle();
    for worker in [2, 7, usize::MAX] {
        let dropped = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicBool::new(false));
        let (guard, flag) = (Counted(Arc::clone(&dropped)), Arc::clone(&ran));
        let err = invalid(h.spawn_on_worker(worker, async move {
            let _guard = guard;
            flag.store(true, Ordering::SeqCst);
        }));
        assert!(err.to_string().contains("does not exist"), "{err}");
        assert_eq!(dropped.load(Ordering::SeqCst), 1, "the future was dropped");
        assert!(!ran.load(Ordering::SeqCst), "the future ran");
    }
}

#[test]
fn a_runtime_with_no_worker_refuses_every_placement() {
    let rt = runtime(0);
    let h = rt.handle();
    invalid(h.spawn_on_worker(0, async {}));
    invalid(h.spawn_local_on_worker(0, || async {}));
    assert!(h.on_each_worker(|| async {}).is_empty());
}

#[test]
fn a_refused_local_placement_never_makes_the_future() {
    let rt = runtime(2);
    let made = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&made);
    invalid(rt.handle().spawn_local_on_worker(5, move || {
        flag.store(true, Ordering::SeqCst);
        async {}
    }));
    assert!(!made.load(Ordering::SeqCst));
}

#[test]
fn a_local_future_is_made_on_its_worker_and_stays_on_one_thread() {
    let rt = runtime(3);
    let h = rt.handle();
    for k in 0..3 {
        let task = h
            .spawn_local_on_worker(k, || {
                let made = (worker_index(), std::thread::current().id());
                let not_send = Rc::new(made);
                async move {
                    nuclei::time::sleep(Duration::from_millis(2)).await;
                    (*not_send, worker_index(), std::thread::current().id())
                }
            })
            .unwrap();
        let ((index, made_on), later_index, later_on) = rt.block_on(task);
        assert_eq!(index, Some(k));
        assert_eq!(later_index, Some(k));
        assert_eq!(made_on, later_on, "a local future moved threads");
    }
}

#[test]
fn on_each_worker_makes_one_task_per_worker_with_distinct_indices() {
    let rt = runtime(3);
    let h = rt.handle();
    let made = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&made);
    let set = h.on_each_worker(move || {
        count.fetch_add(1, Ordering::SeqCst);
        let not_send = Rc::new(worker_index());
        async move { *not_send }
    });
    assert_eq!(set.len(), 3);
    let mut indices = rt.block_on(set.join_all());
    indices.sort();
    assert_eq!(indices, [Some(0), Some(1), Some(2)]);
    assert_eq!(made.load(Ordering::SeqCst), 3);
}

#[test]
fn on_each_worker_of_a_runtime_with_no_worker_is_an_empty_set() {
    let rt = runtime(0);
    let set = rt.handle().on_each_worker(|| async { 1 });
    assert!(set.is_empty());
    assert_eq!(set.len(), 0);
    assert!(rt.block_on(set.join_all()).is_empty());
}

#[test]
fn a_stopped_worker_takes_no_task_and_the_others_go_on() {
    let rt = runtime(1);
    let h = rt.handle();
    rt.block_on(async {
        assert_eq!(nuclei::spawn_more_threads(2).await.unwrap(), 2);
        assert_eq!(h.workers(), 3);
        assert!(
            !nuclei::stop_current_thread().await,
            "block_on's own thread is no worker"
        );
        let stopped = h
            .spawn_on_worker(2, async { nuclei::stop_current_thread().await })
            .unwrap()
            .await;
        assert!(stopped);
        let err = invalid(h.spawn_on_worker(2, async {}));
        assert!(err.to_string().contains("stopped"), "{err}");
        invalid(h.spawn_local_on_worker(2, || async {}));
        assert_eq!(h.workers(), 3, "a stopped worker keeps its place");
        for k in 0..2 {
            let index = h.spawn_on_worker(k, async { worker_index() }).unwrap();
            assert_eq!(index.await, Some(k));
        }
        let mut indices = h
            .on_each_worker(|| async { worker_index() })
            .join_all()
            .await;
        indices.sort();
        assert_eq!(indices, [Some(0), Some(1)], "the stopped worker is skipped");
    });
}

#[test]
fn stop_thread_stops_the_last_worker_and_leaves_the_minimum() {
    let rt = runtime(1);
    let h = rt.handle();
    rt.block_on(async {
        assert_eq!(nuclei::spawn_more_threads(1).await.unwrap(), 1);
        assert_eq!(h.workers(), 2);
        assert!(nuclei::stop_thread().await);
        invalid(h.spawn_on_worker(1, async {}));
        assert!(!nuclei::stop_thread().await, "the minimum keeps running");
        let on_minimum = h
            .spawn_on_worker(0, async { nuclei::stop_current_thread().await })
            .unwrap();
        assert!(!on_minimum.await);
        let index = h.spawn_on_worker(0, async { worker_index() }).unwrap();
        assert_eq!(index.await, Some(0));
    });
}

#[test]
fn a_task_placed_before_a_handoff_keeps_its_worker_and_a_local_future_stays_behind() {
    let rt = on();
    let h = rt.handle();
    let began = Arc::new(AtomicBool::new(false));
    let up = Arc::clone(&began);
    let go = Arc::new(nuclei::sync::Notify::new());
    let wake = Arc::clone(&go);
    let local = h
        .spawn_local_on_worker(0, move || async move {
            let before = worker_index();
            up.store(true, Ordering::SeqCst);
            wake.notified().await;
            (before, worker_index(), std::thread::current().id())
        })
        .unwrap();
    wait_for("the local future's first poll", &began);

    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let blocked_on = Arc::new(OnceLock::<ThreadId>::new());
    let (mine, started) = (Arc::clone(&blocked_on), Arc::new(AtomicBool::new(false)));
    let up = Arc::clone(&started);
    let blocked = h
        .spawn_on_worker(0, async move {
            let _ = mine.set(std::thread::current().id());
            up.store(true, Ordering::SeqCst);
            held.hold();
        })
        .unwrap();
    wait_for("the blocking task", &started);

    let probe = h
        .spawn_on_worker(0, async { (worker_index(), std::thread::current().id()) })
        .unwrap();
    let (index, thread) = rt.block_on(probe);
    gate.release_after("the task placed on the worker");
    let blocked_thread = *blocked_on.get().unwrap();
    assert_eq!(index, Some(0));
    assert_ne!(thread, blocked_thread, "the shard did not move");
    // Only now, with the shard on another thread, may the local future go on.
    go.notify_one();
    rt.block_on(blocked);

    let (before, after, finished_on) = rt.block_on(local);
    assert_eq!(before, Some(0));
    assert_eq!(after, None, "a local future left behind is off the worker");
    assert_eq!(finished_on, blocked_thread);
}

#[test]
fn placements_from_many_threads_reach_the_workers_they_name() {
    let rt = runtime(3);
    let h = rt.handle();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let h = h.clone();
            scope.spawn(move || {
                let tasks: Vec<_> = (0..90)
                    .map(|i| {
                        let k = i % 3;
                        (k, h.spawn_on_worker(k, async { worker_index() }).unwrap())
                    })
                    .collect();
                for (k, task) in tasks {
                    assert_eq!(h.block_on(task), Some(k));
                }
            });
        }
    });
}

#[test]
fn the_task_builder_places_on_the_named_worker() {
    let rt = runtime(3);
    let h = rt.handle();
    for k in 0..3 {
        let task = TaskBuilder::new()
            .worker(k)
            .spawn_on(async { worker_index() }, &h)
            .unwrap();
        assert_eq!(rt.block_on(task), Some(k));
    }
    let inside = rt.block_on(async {
        let task = TaskBuilder::new()
            .worker(2)
            .spawn(async { worker_index() })
            .unwrap();
        task.await
    });
    assert_eq!(inside, Some(2));
}

#[test]
fn the_task_builder_refuses_a_worker_that_is_missing() {
    let rt = runtime(2);
    let h = rt.handle();
    invalid(TaskBuilder::new().worker(2).spawn_on(async {}, &h));
    invalid(TaskBuilder::new().worker(usize::MAX).spawn_on(async {}, &h));
    rt.block_on(async {
        invalid(TaskBuilder::new().worker(5).spawn(async {}));
    });
    let empty = runtime(0);
    invalid(
        TaskBuilder::new()
            .worker(0)
            .spawn_on(async {}, &empty.handle()),
    );
}

#[test]
fn a_local_spawn_with_a_worker_needs_to_run_on_that_worker() {
    let rt = runtime(2);
    let h = rt.handle();
    invalid(TaskBuilder::new().worker(0).spawn_local(async {}));
    rt.block_on(async {
        invalid(TaskBuilder::new().worker(0).spawn_local(async {}));
    });
    let (wrong, right) = rt.block_on(
        h.spawn_on_worker(1, async {
            let wrong = TaskBuilder::new()
                .worker(0)
                .spawn_local(async {})
                .err()
                .map(|err| err.kind());
            let right = TaskBuilder::new()
                .worker(1)
                .spawn_local(async {
                    let here = Rc::new(worker_index());
                    *here
                })
                .unwrap()
                .await;
            (wrong, right)
        })
        .unwrap(),
    );
    assert_eq!(wrong, Some(ErrorKind::InvalidInput));
    assert_eq!(right, Some(1));
}

#[test]
fn a_blocking_spawn_with_a_worker_is_refused_and_without_one_runs() {
    let rt = runtime(2);
    rt.block_on(async {
        let err = invalid(TaskBuilder::new().worker(0).spawn_blocking(|| 1));
        assert!(err.to_string().contains("blocking"), "{err}");
        invalid(TaskBuilder::new().worker(9).spawn_blocking(|| 1));
        let task = TaskBuilder::new().spawn_blocking(|| 3).unwrap();
        assert_eq!(task.await, 3);
    });
}

#[test]
fn a_placed_task_keeps_the_name_the_builder_gave_it() {
    let seen = Arc::new(Mutex::new(Vec::<(nuclei::task::Id, Option<String>)>::new()));
    let record = Arc::clone(&seen);
    let rt = Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .on_task_spawn(move |meta| {
            record
                .lock()
                .unwrap()
                .push((meta.id(), meta.name().map(str::to_owned)));
        })
        .build()
        .unwrap();
    let h = rt.handle();
    let name_of = |id: nuclei::task::Id| {
        seen.lock()
            .unwrap()
            .iter()
            .find(|(got, _)| *got == id)
            .map(|(_, name)| name.clone())
    };

    let named = TaskBuilder::new()
        .name("pinned")
        .worker(1)
        .spawn_on(async { worker_index() }, &h)
        .unwrap();
    assert_eq!(name_of(named.id()), Some(Some("pinned".to_owned())));
    assert_eq!(rt.block_on(named), Some(1));

    invalid(
        TaskBuilder::new()
            .name("ghost")
            .worker(9)
            .spawn_on(async {}, &h),
    );
    let unnamed = rt.spawn(async {});
    assert_eq!(
        name_of(unnamed.id()),
        Some(None),
        "a refused spawn left its name behind"
    );
    rt.block_on(unnamed);
}
