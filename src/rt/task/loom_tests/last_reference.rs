//! The run that completes a task lets go of its reference in the
//! completion's own exchange: a handle polled without a wake that finds the
//! task completed holds the last reference, so its drop frees the task on
//! the handle's thread, wherever the run was (proofs/tla/task,
//! `RunnerFreesNoAwaited`). A blocking job's block is so back in its
//! spawner's cache before the spawner goes on.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};
use std::task::{Context, Poll, Waker};

use loom::sync::{Arc, Mutex};
use loom::thread;

use super::super::{Runnable, Schedule, ScheduleInfo, spawn};
use super::{model, pop};

/// Where the task's memory went: its schedule, dropped with it, says on
/// which thread.
const NOWHERE: usize = 0;
const HANDLE: usize = 1;
const RUNNER: usize = 2;

/// A run queue, and the thread the task was freed on, in a cell loom does
/// not track.
struct Where {
    queue: Arc<Mutex<Vec<Runnable>>>,
    handle: thread::ThreadId,
    freed: std::sync::Arc<StdAtomicUsize>,
}

impl Drop for Where {
    fn drop(&mut self) {
        let on = if thread::current().id() == self.handle {
            HANDLE
        } else {
            RUNNER
        };
        self.freed.store(on, StdOrdering::SeqCst);
    }
}

// SAFETY: the push happens under the lock; nothing of `self` is read after.
unsafe impl Schedule for Where {
    fn schedule(&self, runnable: Runnable, _info: ScheduleInfo) {
        self.queue.lock().unwrap().push(runnable);
    }
}

/// Ready at its first poll.
struct Seven;

impl Future for Seven {
    type Output = u8;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u8> {
        Poll::Ready(7)
    }
}

/// The task completes on a runner thread while this thread polls its handle
/// without being woken, until the output is there, then drops the handle.
fn completion_racing_a_polled_handle() {
    let queue = Arc::new(Mutex::new(Vec::new()));
    let freed = std::sync::Arc::new(StdAtomicUsize::new(NOWHERE));
    let sched = Where {
        queue: queue.clone(),
        handle: thread::current().id(),
        freed: freed.clone(),
    };
    let (runnable, mut task) = spawn(Seven, sched);
    runnable.schedule();
    let runner = {
        let queue = queue.clone();
        thread::spawn(move || {
            while let Some(runnable) = pop(&queue) {
                runnable.run();
            }
        })
    };
    let mut cx = Context::from_waker(Waker::noop());
    let output = loop {
        match Pin::new(&mut task).poll(&mut cx) {
            Poll::Ready(output) => break output,
            Poll::Pending => thread::yield_now(),
        }
    };
    drop(task);
    runner.join().unwrap();
    assert_eq!(output, 7);
    assert_eq!(
        freed.load(StdOrdering::SeqCst),
        HANDLE,
        "the run freed a task whose handle held it as it completed"
    );
}

#[test]
fn a_handle_that_sees_the_completion_frees_the_task_itself() {
    model(
        "a_handle_that_sees_the_completion_frees_the_task_itself",
        10,
        completion_racing_a_polled_handle,
    );
}
