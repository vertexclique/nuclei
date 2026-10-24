//! The handle of a spawned task: [`Task`], and its [`FallibleTask`] form.

use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll};
use std::panic::{RefUnwindSafe, UnwindSafe};

use super::header::Header;
use super::raw::Panic;
use super::state::{AWAITER, CLOSED, COMPLETED, FLAGS, REFERENCE, RUNNING, SCHEDULED, TASK};
use crate::sync::internal::Ordering;

/// A spawned task.
///
/// Awaiting it gives the task's output. Dropping it cancels the task: its
/// future is dropped without being polled again. [`detach`](Self::detach)
/// lets the task run to its end without a handle.
///
/// If the task's future panics, awaiting the handle resumes the panic.
///
/// # Panics
///
/// Awaiting a task that its runtime dropped before it completed (the
/// runtime shut down) panics; [`fallible`](Self::fallible) gives a handle
/// that returns `None` instead.
#[must_use = "dropping a task cancels it; call `detach` to let it run"]
pub struct Task<T> {
    ptr: NonNull<()>,
    _output: PhantomData<T>,
}

// SAFETY: the handle hands the output out by value, so it may move to
// another thread when the output may.
unsafe impl<T: Send> Send for Task<T> {}
// SAFETY: `&Task` only reads the state word.
unsafe impl<T> Sync for Task<T> {}

impl<T> UnwindSafe for Task<T> {}
impl<T> RefUnwindSafe for Task<T> {}

impl<T> Unpin for Task<T> {}

impl<T> Task<T> {
    /// # Safety
    ///
    /// `ptr` is a task with output `T` whose `TASK` flag the handle takes
    /// over.
    pub(super) unsafe fn from_raw(ptr: NonNull<()>) -> Self {
        Self {
            ptr,
            _output: PhantomData,
        }
    }

    fn header(&self) -> &Header {
        // SAFETY: the TASK flag keeps the task alive.
        unsafe { self.ptr.cast::<Header>().as_ref() }
    }

    /// Lets the task run to its end without a handle; its output is dropped.
    pub fn detach(self) {
        let mut this = ManuallyDrop::new(self);
        let output = this.set_detached();
        drop(output);
    }

    /// Cancels the task and waits for its future to be dropped. Returns the
    /// output if the task had already completed.
    pub async fn cancel(self) -> Option<T> {
        let mut this = self;
        this.set_canceled();
        core::future::poll_fn(|cx| this.poll_task(cx)).await
    }

    /// A handle that returns `None` where this one panics: when the task was
    /// cancelled, or its runtime dropped it before it completed.
    pub fn fallible(self) -> FallibleTask<T> {
        FallibleTask { task: self }
    }

    /// Whether the task completed or was cancelled. Awaiting a finished task
    /// returns at once.
    pub fn is_finished(&self) -> bool {
        self.header().state.load(Ordering::Acquire) & (COMPLETED | CLOSED) != 0
    }

    /// Closes the task unless it completed. An idle task is scheduled once
    /// more, so its future is dropped where the task runs.
    fn set_canceled(&mut self) {
        let ptr = self.ptr.as_ptr().cast_const();
        let header = self.header();
        let mut state = header.state.load(Ordering::Acquire);
        loop {
            if state & (COMPLETED | CLOSED) != 0 {
                return;
            }
            let idle = state & (SCHEDULED | RUNNING) == 0;
            let new = if idle {
                (state | SCHEDULED | CLOSED) + REFERENCE
            } else {
                state | CLOSED
            };
            match header.state.compare_exchange_weak(
                state,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if idle {
                        // SAFETY: the reference added above goes to the
                        // runnable, which drops the future when it runs.
                        unsafe { (header.vtable.schedule)(ptr, super::ScheduleInfo::new(false)) };
                    }
                    if state & AWAITER != 0 {
                        header.notify(None);
                    }
                    return;
                }
                Err(actual) => state = actual,
            }
        }
    }

    /// Lets go of the handle. Returns the output when the task had completed
    /// and nobody took it, so the caller drops it outside the transition.
    fn set_detached(&mut self) -> Option<Result<T, Panic>> {
        let ptr = self.ptr.as_ptr().cast_const();
        let header = self.header();
        let mut output = None;
        // A handle detached right after the spawn is the common case: one
        // exchange.
        let Err(mut state) = header.state.compare_exchange_weak(
            SCHEDULED | TASK | REFERENCE,
            SCHEDULED | REFERENCE,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) else {
            return None;
        };
        loop {
            if state & COMPLETED != 0 && state & CLOSED == 0 {
                // Completed and not taken: close it to take the output.
                match header.state.compare_exchange_weak(
                    state,
                    state | CLOSED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        output = Some(self.read_output());
                        state |= CLOSED;
                    }
                    Err(actual) => state = actual,
                }
                continue;
            }
            // The last reference with the task still open: close it and
            // schedule it once more so its future is dropped where it runs.
            let last = state & !FLAGS == 0;
            let new = if last && state & CLOSED == 0 {
                SCHEDULED | CLOSED | REFERENCE
            } else {
                state & !TASK
            };
            match header.state.compare_exchange_weak(
                state,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if last {
                        // SAFETY: with no reference left, the handle decides:
                        // a closed task is freed, an open one gets the
                        // reference set above for its runnable.
                        unsafe {
                            if state & CLOSED == 0 {
                                (header.vtable.schedule)(ptr, super::ScheduleInfo::new(false));
                            } else {
                                (header.vtable.destroy)(ptr);
                            }
                        }
                    }
                    return output;
                }
                Err(actual) => state = actual,
            }
        }
    }

    /// Moves the output out of the cell.
    fn read_output(&self) -> Result<T, Panic> {
        let ptr = self.ptr.as_ptr().cast_const();
        let mut out = MaybeUninit::<Result<T, Panic>>::uninit();
        // SAFETY: the caller closed the completed task, so the output is
        // there and this call alone takes it.
        unsafe {
            (self.header().vtable.read_output)(ptr, out.as_mut_ptr().cast());
            out.assume_init()
        }
    }

    /// `Some(output)` once the task completed, `None` once it was closed
    /// without an output for the handle (cancelled, or dropped by its
    /// runtime) and its future is gone.
    fn poll_task(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let header = self.header();
        let mut state = header.state.load(Ordering::Acquire);
        loop {
            if state & CLOSED != 0 {
                // The future may still be in a run queue or a poll: wait for
                // it to be dropped.
                if state & (SCHEDULED | RUNNING) != 0 {
                    header.register(cx.waker());
                    state = header.state.load(Ordering::Acquire);
                    if state & (SCHEDULED | RUNNING) != 0 {
                        return Poll::Pending;
                    }
                }
                header.notify(Some(cx.waker()));
                return Poll::Ready(None);
            }
            if state & COMPLETED == 0 {
                header.register(cx.waker());
                // The task may have completed or closed just before the
                // registration.
                state = header.state.load(Ordering::Acquire);
                if state & CLOSED != 0 {
                    continue;
                }
                if state & COMPLETED == 0 {
                    return Poll::Pending;
                }
            }
            match header.state.compare_exchange(
                state,
                state | CLOSED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if state & AWAITER != 0 {
                        header.notify(Some(cx.waker()));
                    }
                    return match self.read_output() {
                        Ok(output) => Poll::Ready(Some(output)),
                        Err(panic) => std::panic::resume_unwind(panic),
                    };
                }
                Err(actual) => state = actual,
            }
        }
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.set_canceled();
        drop(self.set_detached());
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        match self.poll_task(cx) {
            Poll::Ready(Some(output)) => Poll::Ready(output),
            Poll::Ready(None) => panic!(
                "the task has no output: it was cancelled or its runtime shut down before it \
                 completed, or it was awaited again after completing"
            ),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.header().state.load(Ordering::Acquire);
        f.debug_struct("Task")
            .field("scheduled", &(state & SCHEDULED != 0))
            .field("running", &(state & RUNNING != 0))
            .field("completed", &(state & COMPLETED != 0))
            .field("closed", &(state & CLOSED != 0))
            .finish()
    }
}

/// A spawned task whose output is `None` when the task ended without one.
///
/// Made by [`Task::fallible`]: awaiting it returns `Some(output)`, or `None`
/// where awaiting the [`Task`] would panic.
#[must_use = "dropping a task cancels it; call `detach` to let it run"]
pub struct FallibleTask<T> {
    task: Task<T>,
}

impl<T> FallibleTask<T> {
    /// Lets the task run to its end without a handle; see [`Task::detach`].
    pub fn detach(self) {
        self.task.detach()
    }

    /// Cancels the task; see [`Task::cancel`].
    pub async fn cancel(self) -> Option<T> {
        self.task.cancel().await
    }

    /// Whether the task completed or was cancelled.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl<T> Future for FallibleTask<T> {
    type Output = Option<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.task.poll_task(cx)
    }
}

impl<T> fmt::Debug for FallibleTask<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FallibleTask").field(&self.task).finish()
    }
}
