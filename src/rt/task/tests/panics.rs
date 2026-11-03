//! A task whose own code panics outside its poll: its future as it is
//! dropped, the output nobody takes, its schedule as its memory goes. No run
//! and no runnable's drop unwinds, the task ends, and the panic goes to the
//! handle that awaits it.

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use super::super::{AbortHandle, Runnable, Schedule, ScheduleInfo, spawn};
use super::{Gate, Sched, gate, open, poll_once, sched};

/// Panics with its message as it is dropped.
struct Bomb(&'static str);

impl Drop for Bomb {
    fn drop(&mut self) {
        std::panic::panic_any(self.0);
    }
}

fn message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .unwrap_or("not a string")
}

/// Ready with 7 once its gate opens (at once without one); panics as it is
/// dropped.
struct Armed {
    gate: Option<Gate>,
    _bomb: Bomb,
}

impl Armed {
    fn ready() -> Self {
        Self {
            gate: None,
            _bomb: Bomb("dropped"),
        }
    }

    fn gated(gate: Gate) -> Self {
        Self {
            gate: Some(gate),
            _bomb: Bomb("dropped"),
        }
    }
}

impl Future for Armed {
    type Output = u32;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        match &mut self.gate {
            Some(gate) => Pin::new(gate).poll(cx).map(|()| 7),
            None => Poll::Ready(7),
        }
    }
}

/// The panic awaiting `task` with `join` resumes.
fn joined_panic<T: core::fmt::Debug>(task: super::Task<T>) -> String {
    let mut join = task.join();
    match poll_once(&mut join) {
        Poll::Ready(Err(err)) => {
            assert!(err.is_panic(), "{err}");
            message(&*err.into_panic()).to_string()
        }
        other => panic!("the handle got {other:?}, not the panic"),
    }
}

#[test]
fn a_future_that_panics_as_it_is_dropped_on_completion_hands_the_panic_to_the_handle() {
    let (s, _queue, dropped) = sched();
    let (runnable, task) = spawn(Armed::ready(), s);
    let ran = catch_unwind(AssertUnwindSafe(|| runnable.run()));
    assert!(matches!(ran, Ok(false)), "the run does not unwind");
    assert_eq!(joined_panic(task), "dropped");
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "the task's memory is freed"
    );
}

#[test]
fn the_polls_own_panic_stays_the_outcome_when_the_drop_panics_too() {
    struct Doomed(#[expect(dead_code, reason = "dropped, not read")] Bomb);
    impl Future for Doomed {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            std::panic::panic_any("polled")
        }
    }
    let (s, _queue, dropped) = sched();
    let (runnable, task) = spawn(Doomed(Bomb("dropped")), s);
    assert!(matches!(
        catch_unwind(AssertUnwindSafe(|| runnable.run())),
        Ok(false)
    ));
    assert_eq!(joined_panic(task), "polled");
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_task_cancelled_in_its_queue_hands_its_futures_drop_panic_to_the_handle() {
    let (s, queue, dropped) = sched();
    let (future, state) = gate();
    let (runnable, task) = spawn(Armed::gated(future), s);
    assert!(!runnable.run(), "pending");
    task.abort();
    assert_eq!(
        queue.len(),
        1,
        "the cancel schedules the idle task once more"
    );
    assert!(
        catch_unwind(AssertUnwindSafe(|| queue.drain())).is_ok(),
        "the run does not unwind"
    );
    assert_eq!(joined_panic(task), "dropped");
    drop(state);
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "the task's memory is freed"
    );
}

#[test]
fn a_task_cancelled_during_its_poll_hands_its_futures_drop_panic_to_the_handle() {
    struct Aborting {
        abort: Arc<Mutex<Option<AbortHandle>>>,
        _bomb: Bomb,
    }
    impl Future for Aborting {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            if let Some(abort) = self.abort.lock().unwrap().take() {
                abort.abort();
            }
            Poll::Pending
        }
    }
    let (s, _queue, dropped) = sched();
    let slot = Arc::new(Mutex::new(None));
    let (runnable, task) = spawn(
        Aborting {
            abort: Arc::clone(&slot),
            _bomb: Bomb("dropped"),
        },
        s,
    );
    *slot.lock().unwrap() = Some(task.abort_handle());
    assert!(matches!(
        catch_unwind(AssertUnwindSafe(|| runnable.run())),
        Ok(false)
    ));
    assert_eq!(joined_panic(task), "dropped");
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_runnable_dropped_unrun_hands_its_futures_drop_panic_to_the_handle() {
    let (s, _queue, dropped) = sched();
    let (runnable, task) = spawn(Armed::ready(), s);
    assert!(
        catch_unwind(AssertUnwindSafe(|| drop(runnable))).is_ok(),
        "the drop does not unwind"
    );
    assert_eq!(joined_panic(task), "dropped");
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_handle_that_cancels_and_waits_gets_the_futures_drop_panic() {
    let (s, queue, dropped) = sched();
    let (future, state) = gate();
    let (runnable, task) = spawn(Armed::gated(future), s);
    assert!(!runnable.run());
    let mut cancel = Box::pin(task.cancel());
    assert!(
        poll_once(&mut cancel).is_pending(),
        "waits for the future's drop"
    );
    assert!(catch_unwind(AssertUnwindSafe(|| queue.drain())).is_ok());
    let awaited = catch_unwind(AssertUnwindSafe(|| poll_once(&mut cancel)));
    let panic = awaited.expect_err("the cancel resumes the drop's panic");
    assert_eq!(message(&*panic), "dropped");
    drop(cancel);
    drop(state);
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "the task's memory is freed"
    );
}

#[test]
fn the_last_waker_of_a_detached_task_leaves_its_futures_drop_panic_to_the_run() {
    let (s, queue, dropped) = sched();
    let (future, state) = gate();
    let (runnable, task) = spawn(Armed::gated(future), s);
    assert!(!runnable.run());
    task.detach();
    drop(open(&state).expect("the poll left its waker"));
    assert_eq!(
        queue.len(),
        1,
        "the last waker schedules the task to drop its future"
    );
    assert!(
        catch_unwind(AssertUnwindSafe(|| queue.drain())).is_ok(),
        "the run does not unwind"
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_detached_task_whose_output_panics_as_it_is_dropped_ends_cleanly() {
    let (s, _queue, dropped) = sched();
    let (runnable, task) = spawn(async { Bomb("output dropped") }, s);
    task.detach();
    assert!(matches!(
        catch_unwind(AssertUnwindSafe(|| runnable.run())),
        Ok(false)
    ));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn a_schedule_that_panics_as_the_task_is_freed_does_not_unwind_the_last_holder() {
    /// The queue's schedule, which panics as it is dropped.
    struct Brittle(Sched);
    impl Drop for Brittle {
        fn drop(&mut self) {
            std::panic::panic_any("freed");
        }
    }
    // SAFETY: as the queue's own schedule.
    unsafe impl Schedule for Brittle {
        fn schedule(&self, runnable: Runnable, info: ScheduleInfo) {
            self.0.schedule(runnable, info);
        }
    }
    let (s, _queue, dropped) = sched();
    let (runnable, mut task) = spawn(async { 7 }, Brittle(s));
    assert!(!runnable.run());
    assert_eq!(poll_once(&mut task), Poll::Ready(7));
    assert!(
        catch_unwind(AssertUnwindSafe(|| drop(task))).is_ok(),
        "the free does not unwind"
    );
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "the rest of the task went"
    );
}
