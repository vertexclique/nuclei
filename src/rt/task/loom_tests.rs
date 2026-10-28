//! Loom models of the task's state word: every race between a run, a wake,
//! the handle's await, cancel and detach, and the drops of the last
//! references.
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --release --lib rt::task::loom_tests
//! ```
//!
//! Each model counts, on plain atomics loom does not track and reads only
//! after every thread joined, how often the future, the output and the
//! task's schedule (so its memory) were dropped: each exactly once. A wake
//! that is lost leaves the task unfinished, which the models assert against;
//! an awaiter that is never woken leaves its thread parked, which loom
//! reports as a deadlock.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};
use std::task::{Context, Poll, Waker};

use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

use super::{Runnable, Schedule, ScheduleInfo, spawn};

/// A drop counter loom does not track.
#[derive(Clone)]
struct Count(std::sync::Arc<StdAtomicUsize>);

impl Count {
    fn new() -> Self {
        Self(std::sync::Arc::new(StdAtomicUsize::new(0)))
    }

    fn get(&self) -> usize {
        self.0.load(StdOrdering::SeqCst)
    }

    fn token(&self) -> Token {
        Token(self.clone())
    }
}

/// Counts one drop on its counter.
struct Token(Count);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.0.fetch_add(1, StdOrdering::SeqCst);
    }
}

/// A run queue behind a loom mutex, and a drop counter for the schedule.
struct Sched {
    queue: Arc<Mutex<Vec<Runnable>>>,
    _token: Token,
    /// The calibration's broken schedule: after the first runnable, every
    /// one goes here, where nothing runs it.
    lost: Option<Arc<Mutex<Vec<Runnable>>>>,
    kept: AtomicBool,
}

impl Sched {
    fn new(queue: &Arc<Mutex<Vec<Runnable>>>, count: &Count) -> Self {
        Self {
            queue: queue.clone(),
            _token: count.token(),
            lost: None,
            kept: AtomicBool::new(false),
        }
    }
}

// SAFETY: the push happens under the lock; nothing of `self` is read after.
unsafe impl Schedule for Sched {
    fn schedule(&self, runnable: Runnable, _info: ScheduleInfo) {
        match &self.lost {
            Some(lost) if self.kept.swap(true, Ordering::AcqRel) => {
                lost.lock().unwrap().push(runnable)
            }
            _ => self.queue.lock().unwrap().push(runnable),
        }
    }
}

fn pop(queue: &Arc<Mutex<Vec<Runnable>>>) -> Option<Runnable> {
    queue.lock().unwrap().pop()
}

/// Ready once `flag` is set; otherwise stores its waker in `slot`.
struct Flagged {
    flag: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Waker>>>,
    _token: Token,
}

impl Future for Flagged {
    type Output = Token;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Token> {
        *self.slot.lock().unwrap() = Some(cx.waker().clone());
        if self.flag.load(Ordering::Acquire) {
            Poll::Ready(self._token.0.token())
        } else {
            Poll::Pending
        }
    }
}

/// Polls `future` on this loom thread, parking until its waker fires.
fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(loom::sync::Notify);
    impl std::task::Wake for Unpark {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.notify();
        }
        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.notify();
        }
    }
    let park = std::sync::Arc::new(Unpark(loom::sync::Notify::new()));
    let waker = Waker::from(std::sync::Arc::clone(&park));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        park.0.wait();
    }
}

/// Runs `f` under loom with at most three preemptions per schedule
/// (`LOOM_MAX_PREEMPTIONS` overrides it), and fails it if the search
/// explored fewer than `floor` interleavings: a model that cannot reach the
/// races it is about passes for the wrong reason.
fn model(name: &str, floor: usize, f: impl Fn() + Sync + Send + 'static) {
    let runs = std::sync::Arc::new(StdAtomicUsize::new(0));
    let counted = {
        let runs = runs.clone();
        move || {
            f();
            runs.fetch_add(1, StdOrdering::Relaxed);
        }
    };
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = builder.preemption_bound.or(Some(3));
    builder.check(counted);
    let explored = runs.load(StdOrdering::Relaxed);
    println!("loom model {name}: {explored} interleavings explored");
    assert!(
        explored >= floor,
        "{name}: only {explored} interleavings, below {floor}"
    );
}

/// A wake from another thread races the run: whatever the order, the task
/// is run again after the flag is set and completes. With `lose_wakes`, the
/// schedule drops every runnable after the first where nothing runs it:
/// the calibration, which must fail.
fn wake_racing_poll(lose_wakes: bool) {
    let (futures, scheds) = (Count::new(), Count::new());
    let queue = Arc::new(Mutex::new(Vec::new()));
    let lost = Arc::new(Mutex::new(Vec::new()));
    let flag = Arc::new(AtomicBool::new(false));
    let slot = Arc::new(Mutex::new(None::<Waker>));
    let future = Flagged {
        flag: flag.clone(),
        slot: slot.clone(),
        _token: futures.token(),
    };
    let mut sched = Sched::new(&queue, &scheds);
    if lose_wakes {
        sched.lost = Some(lost.clone());
    }
    let (runnable, task) = spawn(future, sched);
    runnable.schedule();

    let waker_thread = {
        let (flag, slot) = (flag.clone(), slot.clone());
        thread::spawn(move || {
            flag.store(true, Ordering::Release);
            if let Some(waker) = slot.lock().unwrap().take() {
                waker.wake();
            }
        })
    };
    // Run whatever is queued while the other thread works, then once it is
    // done: a wake it made is queued by then.
    while let Some(runnable) = pop(&queue) {
        runnable.run();
    }
    waker_thread.join().unwrap();
    while let Some(runnable) = pop(&queue) {
        runnable.run();
    }
    let finished = task.is_finished();
    // Everything goes before the verdict: a loom object dropped while a
    // failed assertion unwinds aborts the process.
    drop(task);
    lost.lock().unwrap().clear();
    slot.lock().unwrap().take();
    assert!(finished, "the wake after the flag was lost");
    assert_eq!(futures.get(), 2, "the future, and the output token it made");
    assert_eq!(scheds.get(), 1, "the memory went once");
}

#[test]
fn a_wake_racing_the_poll_is_never_lost() {
    model("a_wake_racing_the_poll_is_never_lost", 200, || {
        wake_racing_poll(false)
    });
}

/// The model above with a schedule that loses every wake after the first:
/// the search must reach an order where the task needed that wake.
#[test]
#[should_panic(expected = "the wake after the flag was lost")]
fn calibration_a_schedule_that_loses_wakes_is_caught() {
    model(
        "calibration_a_schedule_that_loses_wakes_is_caught",
        1,
        || wake_racing_poll(true),
    );
}

/// Dropping the handle on another thread races the run: the future is
/// dropped once, by whichever side owns it, and the memory goes once.
#[test]
fn a_cancel_racing_the_run_drops_the_future_once() {
    model("a_cancel_racing_the_run_drops_the_future_once", 70, || {
        let (futures, scheds) = (Count::new(), Count::new());
        let queue = Arc::new(Mutex::new(Vec::new()));
        let guard = futures.token();
        let (runnable, task) = spawn(
            async move {
                let _guard = guard;
                std::future::pending::<()>().await
            },
            Sched::new(&queue, &scheds),
        );
        let canceller = thread::spawn(move || drop(task));
        runnable.run();
        canceller.join().unwrap();
        while let Some(runnable) = pop(&queue) {
            runnable.run();
        }
        assert_eq!(futures.get(), 1);
        assert_eq!(scheds.get(), 1);
    });
}

/// The handle awaits on one thread while the task completes on another: the
/// awaiter is woken (or sees the output at once) and gets the output.
#[test]
fn an_awaiter_racing_the_completion_is_woken() {
    model("an_awaiter_racing_the_completion_is_woken", 3000, || {
        let scheds = Count::new();
        let queue = Arc::new(Mutex::new(Vec::new()));
        let (runnable, task) = spawn(async { 7 }, Sched::new(&queue, &scheds));
        let runner = thread::spawn(move || {
            runnable.run();
        });
        assert_eq!(block_on(task), 7);
        runner.join().unwrap();
        assert_eq!(scheds.get(), 1);
    });
}

/// Detaching on one thread while the task completes on another: the output
/// is dropped once, by the run or by the detach.
#[test]
fn a_detach_racing_the_completion_drops_the_output_once() {
    model(
        "a_detach_racing_the_completion_drops_the_output_once",
        35,
        || {
            let (outputs, scheds) = (Count::new(), Count::new());
            let queue = Arc::new(Mutex::new(Vec::new()));
            let out = outputs.token();
            let (runnable, task) = spawn(async move { out }, Sched::new(&queue, &scheds));
            let detacher = thread::spawn(move || task.detach());
            runnable.run();
            detacher.join().unwrap();
            assert_eq!(outputs.get(), 1);
            assert_eq!(scheds.get(), 1);
        },
    );
}

/// The last waker of an idle task is dropped on one thread while the handle
/// is detached on another: whoever is last closes the task and schedules it,
/// and its run drops the future once.
#[test]
fn the_last_waker_racing_a_detach_drops_the_future_once() {
    model(
        "the_last_waker_racing_a_detach_drops_the_future_once",
        8,
        || {
            let (futures, scheds) = (Count::new(), Count::new());
            let queue = Arc::new(Mutex::new(Vec::new()));
            let slot = Arc::new(Mutex::new(None::<Waker>));
            let flag = Arc::new(AtomicBool::new(false));
            let (runnable, task) = spawn(
                Flagged {
                    flag,
                    slot: slot.clone(),
                    _token: futures.token(),
                },
                Sched::new(&queue, &scheds),
            );
            runnable.run();
            let waker = slot
                .lock()
                .unwrap()
                .take()
                .expect("the poll left its waker");
            let dropper = thread::spawn(move || drop(waker));
            task.detach();
            dropper.join().unwrap();
            while let Some(runnable) = pop(&queue) {
                runnable.run();
            }
            assert_eq!(futures.get(), 1);
            assert_eq!(scheds.get(), 1);
        },
    );
}

/// A wake by reference from another thread races the handle's drop: the
/// task is cancelled and its future dropped once, whether the wake came
/// first or not.
#[test]
fn a_wake_racing_a_cancel_drops_the_future_once() {
    model("a_wake_racing_a_cancel_drops_the_future_once", 40, || {
        let (futures, scheds) = (Count::new(), Count::new());
        let queue = Arc::new(Mutex::new(Vec::new()));
        let slot = Arc::new(Mutex::new(None::<Waker>));
        let flag = Arc::new(AtomicBool::new(false));
        let (runnable, task) = spawn(
            Flagged {
                flag,
                slot: slot.clone(),
                _token: futures.token(),
            },
            Sched::new(&queue, &scheds),
        );
        runnable.run();
        let waker = slot
            .lock()
            .unwrap()
            .take()
            .expect("the poll left its waker");
        let waking = thread::spawn(move || waker.wake_by_ref());
        drop(task);
        waking.join().unwrap();
        while let Some(runnable) = pop(&queue) {
            runnable.run();
        }
        assert_eq!(futures.get(), 1);
        assert_eq!(scheds.get(), 1);
    });
}

/// The handle is detached; another thread drops the task's only waker while
/// the run polls. Whoever lets go last (the waker's drop, closing an idle
/// task and scheduling it, or the run's own drop) drops the future, once.
#[test]
fn the_last_waker_dropped_during_the_poll_drops_the_future_once() {
    model(
        "the_last_waker_dropped_during_the_poll_drops_the_future_once",
        25,
        || {
            let (futures, scheds) = (Count::new(), Count::new());
            let queue = Arc::new(Mutex::new(Vec::new()));
            let slot = Arc::new(Mutex::new(None::<Waker>));
            let flag = Arc::new(AtomicBool::new(false));
            let (runnable, task) = spawn(
                Flagged {
                    flag,
                    slot: slot.clone(),
                    _token: futures.token(),
                },
                Sched::new(&queue, &scheds),
            );
            task.detach();
            let dropper = {
                let slot = slot.clone();
                thread::spawn(move || drop(slot.lock().unwrap().take()))
            };
            runnable.run();
            dropper.join().unwrap();
            // A waker the poll stored after the other thread looked goes now.
            drop(slot.lock().unwrap().take());
            while let Some(runnable) = pop(&queue) {
                runnable.run();
            }
            assert_eq!(futures.get(), 1, "the future was dropped once");
            assert_eq!(scheds.get(), 1, "the memory went once");
        },
    );
}
