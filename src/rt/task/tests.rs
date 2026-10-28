use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use super::join::Task;
use super::{Runnable, Schedule, ScheduleInfo, spawn, spawn_unchecked};

/// A run queue for one test: what was scheduled, in order, with how.
#[derive(Clone, Default)]
struct Queue(Arc<Mutex<Vec<(Runnable, ScheduleInfo)>>>);

impl Queue {
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn pop(&self) -> Option<(Runnable, ScheduleInfo)> {
        let mut q = self.0.lock().unwrap();
        (!q.is_empty()).then(|| q.remove(0))
    }

    /// Runs what is queued until nothing is; returns how many runs.
    fn drain(&self) -> usize {
        let mut runs = 0;
        while let Some((runnable, _)) = self.pop() {
            runnable.run();
            runs += 1;
        }
        runs
    }
}

/// A queue's schedule, counting its own drops: the task's memory goes when
/// its schedule does.
struct Sched {
    queue: Queue,
    dropped: Arc<AtomicUsize>,
}

impl Drop for Sched {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

// SAFETY: the push happens under the lock, and nothing of `self` is read
// after it.
unsafe impl Schedule for Sched {
    fn schedule(&self, runnable: Runnable, info: ScheduleInfo) {
        self.queue.0.lock().unwrap().push((runnable, info));
    }
}

fn sched() -> (Sched, Queue, Arc<AtomicUsize>) {
    let queue = Queue::default();
    let dropped = Arc::new(AtomicUsize::new(0));
    (
        Sched {
            queue: queue.clone(),
            dropped: dropped.clone(),
        },
        queue,
        dropped,
    )
}

fn noop_cx() -> Context<'static> {
    Context::from_waker(Waker::noop())
}

fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut noop_cx())
}

/// Counts its drops.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A future that is pending until `ready` is set, keeping the last waker.
struct Gate {
    ready: Arc<Mutex<(bool, Option<Waker>)>>,
}

impl Future for Gate {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut g = self.ready.lock().unwrap();
        if g.0 {
            Poll::Ready(())
        } else {
            g.1 = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

type GateState = Arc<Mutex<(bool, Option<Waker>)>>;

fn gate() -> (Gate, GateState) {
    let ready = Arc::new(Mutex::new((false, None)));
    (
        Gate {
            ready: ready.clone(),
        },
        ready,
    )
}

fn open(state: &GateState) -> Option<Waker> {
    let mut g = state.lock().unwrap();
    g.0 = true;
    g.1.take()
}

#[test]
fn a_task_runs_once_and_its_handle_gets_the_output() {
    let (s, _queue, dropped) = sched();
    let (runnable, mut task) = spawn(async { 7 }, s);
    assert!(!task.is_finished());
    assert!(!runnable.run());
    assert!(task.is_finished());
    assert_eq!(poll_once(&mut task), Poll::Ready(7));
    drop(task);
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "the task's memory is freed"
    );
}

#[test]
fn a_pending_task_is_scheduled_again_by_its_waker() {
    let (s, queue, dropped) = sched();
    let (future, state) = gate();
    let (runnable, mut task) = spawn(future, s);
    runnable.run();
    assert_eq!(queue.len(), 0, "pending, and nothing woke it");
    assert!(poll_once(&mut task).is_pending());

    open(&state).expect("the poll left its waker").wake();
    let (runnable, info) = queue.pop().expect("the wake scheduled it");
    assert!(!info.woken_while_running);
    runnable.run();
    assert_eq!(poll_once(&mut task), Poll::Ready(()));
    drop(task);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_wake_during_the_poll_reschedules_after_it() {
    let (s, queue, _dropped) = sched();
    let polls = Arc::new(AtomicUsize::new(0));
    let p = polls.clone();
    let (runnable, mut task) = spawn(
        core::future::poll_fn(move |cx| {
            if p.fetch_add(1, Ordering::SeqCst) == 0 {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }),
        s,
    );
    assert!(runnable.run(), "it woke itself, so the run rescheduled it");
    assert_eq!(
        polls.load(Ordering::SeqCst),
        1,
        "not polled twice in one run"
    );
    let (runnable, info) = queue.pop().expect("rescheduled once");
    assert!(info.woken_while_running);
    assert_eq!(queue.len(), 0);
    assert!(!runnable.run());
    assert_eq!(poll_once(&mut task), Poll::Ready(()));
}

#[test]
fn a_task_woken_twice_before_it_runs_is_queued_once() {
    let (s, queue, _dropped) = sched();
    let (future, state) = gate();
    let (runnable, task) = spawn(future, s);
    runnable.run();
    let waker = open(&state).unwrap();
    waker.wake_by_ref();
    waker.wake_by_ref();
    waker.wake();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.drain(), 1);
    assert!(task.is_finished());
}

#[test]
fn a_wake_after_completion_does_nothing() {
    let (s, queue, dropped) = sched();
    let (future, state) = gate();
    let (runnable, task) = spawn(future, s);
    runnable.run();
    let waker = open(&state).unwrap();
    let spare = waker.clone();
    waker.wake();
    queue.drain();
    assert!(task.is_finished());
    spare.wake_by_ref();
    assert_eq!(queue.len(), 0);
    drop(task);
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        0,
        "the spare waker keeps the memory"
    );
    drop(spare);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_panicking_poll_completes_the_task_and_the_handle_resumes_it() {
    let (s, _queue, _dropped) = sched();
    let (runnable, mut task) = spawn(async { panic!("boom") }, s);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runnable.run()));
    assert!(matches!(ran, Ok(false)), "the run itself does not unwind");
    assert!(task.is_finished());
    let awaited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll_once(&mut task)));
    std::panic::set_hook(hook);
    let payload = awaited.expect_err("awaiting resumes the panic");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"boom"));
}

#[test]
fn a_detached_task_that_panics_drops_the_panic() {
    let (s, _queue, dropped) = sched();
    let (runnable, task) = spawn(async { panic!("boom") }, s);
    task.detach();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runnable.run()));
    std::panic::set_hook(hook);
    assert!(matches!(ran, Ok(false)));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn dropping_the_handle_of_an_idle_task_drops_its_future_where_it_runs() {
    let (s, queue, dropped) = sched();
    let futures = Arc::new(AtomicUsize::new(0));
    let guard = Counted(futures.clone());
    let (future, state) = gate();
    let (runnable, task) = spawn(
        async move {
            let _guard = guard;
            future.await
        },
        s,
    );
    runnable.run();
    drop(task);
    assert_eq!(
        futures.load(Ordering::SeqCst),
        0,
        "not dropped by the handle"
    );
    assert_eq!(queue.len(), 1, "scheduled once more to drop its future");
    queue.drain();
    assert_eq!(futures.load(Ordering::SeqCst), 1);
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        0,
        "the gate's waker keeps the memory"
    );
    drop(state);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn dropping_the_handle_of_a_scheduled_task_drops_the_future_at_its_run() {
    let (s, queue, dropped) = sched();
    let futures = Arc::new(AtomicUsize::new(0));
    let guard = Counted(futures.clone());
    let polled = Arc::new(AtomicUsize::new(0));
    let p = polled.clone();
    let (runnable, task) = spawn(
        async move {
            let _guard = guard;
            p.fetch_add(1, Ordering::SeqCst);
        },
        s,
    );
    drop(task);
    assert_eq!(queue.len(), 0, "already scheduled: no second runnable");
    runnable.run();
    assert_eq!(
        polled.load(Ordering::SeqCst),
        0,
        "a cancelled task is not polled"
    );
    assert_eq!(futures.load(Ordering::SeqCst), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_detached_task_runs_to_its_end_and_drops_its_output() {
    let (s, queue, dropped) = sched();
    let outputs = Arc::new(AtomicUsize::new(0));
    let out = Counted(outputs.clone());
    let (future, state) = gate();
    let (runnable, task) = spawn(
        async move {
            future.await;
            out
        },
        s,
    );
    runnable.run();
    task.detach();
    open(&state).unwrap().wake();
    queue.drain();
    assert_eq!(
        outputs.load(Ordering::SeqCst),
        1,
        "no handle, so the run drops it"
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_handle_dropped_after_completion_drops_the_output_once() {
    let (s, _queue, dropped) = sched();
    let outputs = Arc::new(AtomicUsize::new(0));
    let out = Counted(outputs.clone());
    let (runnable, task) = spawn(async move { out }, s);
    runnable.run();
    assert_eq!(outputs.load(Ordering::SeqCst), 0, "kept for the handle");
    drop(task);
    assert_eq!(outputs.load(Ordering::SeqCst), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn cancel_returns_the_output_of_a_completed_task() {
    let (s, _queue, _dropped) = sched();
    let (runnable, task) = spawn(async { 7 }, s);
    runnable.run();
    let mut cancel = Box::pin(task.cancel());
    assert_eq!(cancel.as_mut().poll(&mut noop_cx()), Poll::Ready(Some(7)));
}

#[test]
fn cancel_of_a_pending_task_waits_for_its_future_to_be_dropped() {
    let (s, queue, dropped) = sched();
    let futures = Arc::new(AtomicUsize::new(0));
    let guard = Counted(futures.clone());
    let (future, state) = gate();
    let (runnable, task) = spawn(
        async move {
            let _guard = guard;
            future.await
        },
        s,
    );
    runnable.run();
    let mut cancel = Box::pin(task.cancel());
    assert!(cancel.as_mut().poll(&mut noop_cx()).is_pending());
    queue.drain();
    assert_eq!(futures.load(Ordering::SeqCst), 1);
    assert_eq!(cancel.as_mut().poll(&mut noop_cx()), Poll::Ready(None));
    drop(cancel);
    drop(state);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_dropped_runnable_cancels_the_task() {
    let (s, _queue, dropped) = sched();
    let futures = Arc::new(AtomicUsize::new(0));
    let guard = Counted(futures.clone());
    let (runnable, task) = spawn(
        async move {
            let _guard = guard;
        },
        s,
    );
    drop(runnable);
    assert_eq!(
        futures.load(Ordering::SeqCst),
        1,
        "dropped by the runnable's drop"
    );
    let mut task = task.fallible();
    assert_eq!(poll_once(&mut task), Poll::Ready(None));
    drop(task);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn awaiting_a_task_whose_runnable_was_dropped_panics() {
    let (s, _queue, _dropped) = sched();
    let (runnable, mut task) = spawn(async { 1 }, s);
    drop(runnable);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let awaited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll_once(&mut task)));
    std::panic::set_hook(hook);
    assert!(awaited.is_err());
}

#[test]
fn the_handle_is_woken_when_the_task_completes() {
    let (s, queue, _dropped) = sched();
    let (future, state) = gate();
    let (runnable, mut task) = spawn(future, s);
    runnable.run();

    let woken = Arc::new(AtomicUsize::new(0));
    struct Flag(Arc<AtomicUsize>);
    impl std::task::Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let waker = Waker::from(Arc::new(Flag(woken.clone())));
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
    open(&state).unwrap().wake();
    queue.drain();
    assert_eq!(woken.load(Ordering::SeqCst), 1);
    assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Ready(()));
}

#[test]
fn every_waker_of_a_task_will_wake_the_others() {
    let (s, queue, dropped) = sched();
    let seen: Arc<Mutex<Vec<Waker>>> = Arc::default();
    let keep = seen.clone();
    let (runnable, task) = spawn(
        core::future::poll_fn(move |cx| {
            keep.lock().unwrap().push(cx.waker().clone());
            let poll_waker = cx.waker();
            assert!(poll_waker.will_wake(&keep.lock().unwrap()[0]));
            Poll::<()>::Pending
        }),
        s,
    );
    let outside = runnable.waker();
    runnable.run();
    {
        let seen = seen.lock().unwrap();
        assert!(
            outside.will_wake(&seen[0]),
            "a runnable's waker and the poll's are one"
        );
        assert!(seen[0].will_wake(&seen[0].clone()));
    }
    // The future holds wakers of its own task: cancel it, let the run drop
    // the future, then let go of the wakers.
    drop(task);
    queue.drain();
    seen.lock().unwrap().clear();
    drop(outside);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn the_last_waker_of_a_detached_idle_task_drops_its_future() {
    let (s, queue, dropped) = sched();
    let futures = Arc::new(AtomicUsize::new(0));
    let guard = Counted(futures.clone());
    let (future, state) = gate();
    let (runnable, task) = spawn(
        async move {
            let _guard = guard;
            future.await
        },
        s,
    );
    runnable.run();
    task.detach();
    // The gate holds the only waker; nothing can wake the task once it goes.
    let waker = state.lock().unwrap().1.take().unwrap();
    drop(waker);
    assert_eq!(queue.len(), 1, "closed and scheduled to drop its future");
    queue.drain();
    assert_eq!(futures.load(Ordering::SeqCst), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_wake_from_another_thread_goes_through_the_schedule() {
    let (s, queue, _dropped) = sched();
    let (future, state) = gate();
    let (runnable, task) = spawn(future, s);
    runnable.run();
    let waker = open(&state).unwrap();
    std::thread::spawn(move || waker.wake()).join().unwrap();
    assert_eq!(queue.drain(), 1);
    assert!(task.is_finished());
}

#[test]
fn a_task_of_a_future_that_is_not_send_runs_on_its_thread() {
    let (s, queue, dropped) = sched();
    let local = std::rc::Rc::new(Cell::new(0));
    let l = local.clone();
    // SAFETY: the runnable is run and dropped on this thread only.
    let (runnable, mut task) = unsafe {
        spawn_unchecked(
            async move {
                l.set(l.get() + 1);
                l.get()
            },
            s,
        )
    };
    runnable.run();
    assert_eq!(queue.len(), 0);
    assert_eq!(poll_once(&mut task), Poll::Ready(1));
    assert_eq!(local.get(), 1);
    drop(task);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_runnable_survives_a_round_trip_through_its_raw_pointer() {
    let (s, _queue, _dropped) = sched();
    let (runnable, mut task) = spawn(async { 3 }, s);
    let raw = runnable.into_raw();
    // SAFETY: the pointer came from `into_raw` and carries its reference.
    let runnable = unsafe { Runnable::from_raw(raw) };
    runnable.run();
    assert_eq!(poll_once(&mut task), Poll::Ready(3));
}

#[test]
fn tasks_run_from_many_threads_complete_once_each() {
    let (s, queue, dropped) = sched();
    drop(s);
    let n = if cfg!(miri) { 8 } else { 2_000 };
    let mut tasks: Vec<Task<usize>> = Vec::new();
    for i in 0..n {
        let s = Sched {
            queue: queue.clone(),
            dropped: dropped.clone(),
        };
        let (runnable, task) = spawn(async move { i }, s);
        runnable.schedule();
        tasks.push(task);
    }
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let queue = queue.clone();
            std::thread::spawn(move || queue.drain())
        })
        .collect();
    let runs: usize = workers.into_iter().map(|w| w.join().unwrap()).sum();
    assert_eq!(runs, n);
    for (i, mut task) in tasks.into_iter().enumerate() {
        assert_eq!(poll_once(&mut task), Poll::Ready(i));
    }
    assert_eq!(dropped.load(Ordering::SeqCst), n + 1);
}

/// The future drops the task's last waker during its own poll, with the
/// handle detached: nothing can wake the task or await it again, so the run
/// drops the future instead of leaving it behind with the memory.
#[test]
fn a_detached_task_whose_last_waker_goes_during_its_poll_drops_its_future() {
    let (s, queue, dropped) = sched();
    let futures = Arc::new(AtomicUsize::new(0));
    let guard = Counted(futures.clone());
    let slot: Arc<Mutex<Option<Waker>>> = Arc::default();
    let keep = slot.clone();
    let polls = Arc::new(AtomicUsize::new(0));
    let p = polls.clone();
    let (runnable, task) = spawn(
        async move {
            let _guard = guard;
            core::future::poll_fn(move |cx| {
                if p.fetch_add(1, Ordering::SeqCst) == 0 {
                    *keep.lock().unwrap() = Some(cx.waker().clone());
                } else {
                    // The last waker goes during the poll.
                    keep.lock().unwrap().take();
                }
                Poll::<()>::Pending
            })
            .await
        },
        s,
    );
    runnable.run();
    task.detach();
    slot.lock().unwrap().as_ref().unwrap().wake_by_ref();
    assert_eq!(queue.drain(), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(
        futures.load(Ordering::SeqCst),
        1,
        "the future was dropped, not left behind"
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}
