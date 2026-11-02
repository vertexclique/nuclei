//! Every task ends when its runtime shuts down or its thread exits: one pending for good has its
//! future dropped, on its own thread, and its handle resolves as cancelled; a drop does not wait
//! forever for a thread blocked in a poll. Nothing is timed against a bound smaller than seconds.

use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use nuclei::runtime::{Builder, Runtime};

const SAFETY: Duration = Duration::from_secs(10);

/// Records that it dropped, and on which thread.
#[derive(Clone, Default)]
struct Mark {
    dropped: Arc<AtomicBool>,
    thread: Arc<Mutex<Option<ThreadId>>>,
}

struct Guard(Mark);

impl Mark {
    fn guard(&self) -> Guard {
        Guard(self.clone())
    }

    fn dropped_on(&self) -> Option<ThreadId> {
        assert!(
            self.dropped.load(Ordering::SeqCst),
            "the future was never dropped"
        );
        *self.thread.lock().unwrap()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        *self.0.thread.lock().unwrap() = Some(std::thread::current().id());
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}

/// Pending for good, holding a waker of its task out of reach: a task nobody can wake is dropped
/// as its poll returns, so this one stays live.
struct Held(Arc<Mutex<Option<Waker>>>);

impl Future for Held {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        *self.0.lock().unwrap() = Some(cx.waker().clone());
        Poll::Pending
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

fn wait_for<T>(rx: &std::sync::mpsc::Receiver<T>) -> T {
    rx.recv_timeout(SAFETY).expect("the task never ran")
}

/// Awaits a handle whose runtime is gone: the task was cancelled.
fn cancelled<T: 'static>(task: nuclei::Task<T>) {
    let out = nuclei::block_on(task.join());
    assert!(
        out.is_err_and(|e| e.is_cancelled()),
        "the handle did not resolve as cancelled"
    );
}

type End = fn(Runtime);

fn dropped(rt: Runtime) {
    drop(rt);
}

fn timed(rt: Runtime) {
    rt.shutdown_timeout(Duration::from_secs(5));
}

fn a_worker_task_pending_for_good_ends(end: End) {
    let before = nuclei::stats().tasks_left;
    let rt = runtime(None);
    let mark = Mark::default();
    let (tx, rx) = std::sync::mpsc::channel();
    let guard = mark.guard();
    let task = rt.spawn(async move {
        let _guard = guard;
        tx.send(std::thread::current().id()).unwrap();
        pending::<()>().await;
    });
    let worker = wait_for(&rx);
    end(rt);
    assert_eq!(
        mark.dropped_on(),
        Some(worker),
        "the future was dropped off its worker"
    );
    cancelled(task);
    assert_eq!(nuclei::stats().tasks_left, before);
}

fn a_bound_task_pending_for_good_ends(end: End) {
    let before = nuclei::stats().tasks_left;
    let rt = runtime(None);
    let mark = Mark::default();
    let (tx, rx) = std::sync::mpsc::channel();
    let guard = mark.guard();
    let (polled_tx, polled) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let bound = nuclei::spawn_local(async move {
            let _guard = guard;
            polled_tx.send(()).unwrap();
            pending::<()>().await;
        });
        tx.send((std::thread::current().id(), bound)).unwrap();
    })
    .detach();
    let (worker, bound) = wait_for(&rx);
    // Polled to pending once: it is listed, not queued.
    wait_for(&polled);
    end(rt);
    assert_eq!(
        mark.dropped_on(),
        Some(worker),
        "the bound future was dropped off its thread"
    );
    cancelled(bound);
    assert_eq!(nuclei::stats().tasks_left, before);
}

#[test]
fn a_dropped_runtime_ends_a_worker_task_pending_for_good() {
    a_worker_task_pending_for_good_ends(dropped);
}

#[test]
fn a_timed_shutdown_ends_a_worker_task_pending_for_good() {
    a_worker_task_pending_for_good_ends(timed);
}

#[test]
fn a_dropped_runtime_ends_a_bound_task_pending_for_good() {
    a_bound_task_pending_for_good_ends(dropped);
}

#[test]
fn a_timed_shutdown_ends_a_bound_task_pending_for_good() {
    a_bound_task_pending_for_good_ends(timed);
}

#[test]
fn a_thread_that_exits_ends_its_bound_tasks() {
    let before = nuclei::stats().tasks_left;
    let mark = Mark::default();
    let guard = mark.guard();
    let waker = Arc::new(Mutex::new(None));
    let held = Held(Arc::clone(&waker));
    let thread = std::thread::spawn(move || {
        nuclei::block_on(async move {
            let polled = Arc::new(AtomicBool::new(false));
            let seen = Arc::clone(&polled);
            nuclei::spawn_local(async move {
                let _guard = guard;
                seen.store(true, Ordering::SeqCst);
                held.await;
            })
            .detach();
            // Polled to pending once: it is listed, not queued.
            while !polled.load(Ordering::SeqCst) {
                nuclei::sleep(Duration::from_millis(1)).await;
            }
        });
        std::thread::current().id()
    });
    let me = thread.join().unwrap();
    assert_eq!(
        mark.dropped_on(),
        Some(me),
        "the future was dropped off its thread"
    );
    assert_eq!(nuclei::stats().tasks_left, before);
    drop(waker.lock().unwrap().take());
}

/// A block a task holds its thread at until the test opens it, or the safety passed.
#[derive(Default)]
struct Gate(AtomicBool);

impl Gate {
    fn hold(&self) {
        let deadline = Instant::now() + SAFETY;
        while !self.0.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[test]
fn a_shutdown_does_not_wait_forever_for_a_thread_blocked_in_a_poll() {
    let before = nuclei::stats().tasks_left;
    let rt = runtime(Some(Duration::from_millis(10)));
    let gate = Arc::new(Gate::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let held = Arc::clone(&gate);
    rt.spawn(async move {
        tx.send(()).unwrap();
        held.hold();
    })
    .detach();
    wait_for(&rx);
    // Another task of the shard finishes only once the shard was handed on.
    let other = rt.spawn(async { 7u32 });
    let out = rt.block_on(nuclei::timeout(SAFETY, other.join()));
    assert!(matches!(out, Ok(Ok(7))), "the shard was not handed on");
    let started = Instant::now();
    rt.shutdown_timeout(Duration::from_millis(200));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the shutdown waited for the blocked poll"
    );
    gate.0.store(true, Ordering::SeqCst);
    assert_eq!(nuclei::stats().tasks_left, before);
}

#[test]
fn a_thread_back_from_a_taken_poll_ends_its_bound_tasks_at_the_close() {
    let before = nuclei::stats().tasks_left;
    let rt = runtime(Some(Duration::from_millis(10)));
    let gate = Arc::new(Gate::default());
    let mark = Mark::default();
    let guard = mark.guard();
    let (tx, rx) = std::sync::mpsc::channel();
    let (polled_tx, polled) = std::sync::mpsc::channel();
    let held = Arc::clone(&gate);
    rt.spawn(async move {
        let bound = nuclei::spawn_local(async move {
            let _guard = guard;
            polled_tx.send(std::thread::current().id()).unwrap();
            pending::<()>().await;
        });
        tx.send(bound).unwrap();
        held.hold();
    })
    .detach();
    let bound = wait_for(&rx);
    let other = rt.spawn(async { 7u32 });
    let out = rt.block_on(nuclei::timeout(SAFETY, other.join()));
    assert!(matches!(out, Ok(Ok(7))), "the shard was not handed on");
    gate.0.store(true, Ordering::SeqCst);
    // The thread, back from its poll, ran the bound task on its own shard.
    let thread = wait_for(&polled);
    drop(rt);
    assert_eq!(
        mark.dropped_on(),
        Some(thread),
        "the bound future was dropped off its thread"
    );
    cancelled(bound);
    assert_eq!(nuclei::stats().tasks_left, before);
}
