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

    /// Closes the task unless it completed; see [`cancel`].
    fn set_canceled(&mut self) {
        // SAFETY: the TASK flag keeps the task alive.
        unsafe { cancel(self.ptr.as_ptr().cast_const()) }
    }

    /// The task's id.
    pub fn id(&self) -> super::Id {
        self.header().id
    }

    /// Cancels the task without waiting: its future is dropped where it runs,
    /// and awaiting the handle then finds no output (unless the task had
    /// completed). [`join`](Self::join) reports it as a cancelled
    /// [`JoinError`].
    pub fn abort(&self) {
        // SAFETY: the TASK flag keeps the task alive.
        unsafe { cancel(self.ptr.as_ptr().cast_const()) }
    }

    /// A handle that cancels the task from anywhere, without the [`Task`]:
    /// it can be cloned and sent to another thread, and outlives the handle.
    pub fn abort_handle(&self) -> AbortHandle {
        let ptr = self.ptr.as_ptr().cast_const();
        let header = self.header();
        // A waker clone holds one reference of the task, and its drop, the
        // last one, drops an open task's future where the task runs (never on
        // the dropping thread): what an abort handle needs.
        // SAFETY: the TASK flag keeps the task alive while the clone is made.
        let waker = core::mem::ManuallyDrop::new(unsafe {
            core::task::Waker::from_raw(core::task::RawWaker::new(ptr, &header.vtable.waker))
        });
        AbortHandle {
            waker: (*waker).clone(),
            id: header.id,
        }
    }

    /// A future that gives the task's output, or a [`JoinError`] saying why
    /// there is none: the task was cancelled (its runtime dropped it too), or
    /// its future panicked, with the payload.
    pub fn join(self) -> Join<T> {
        Join { task: self }
    }
}

/// Closes the task `ptr` unless it completed. An idle task is scheduled once
/// more, so its future is dropped where the task runs, and an awaiter is told.
///
/// # Safety
///
/// `ptr` is a live task (its handle or a reference keeps it alive).
unsafe fn cancel(ptr: *const ()) {
    // SAFETY: the caller's contract.
    let header = unsafe { &*ptr.cast::<Header>() };
    {
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
}

impl<T> Task<T> {
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

    /// `Some(output)` once the task completed (resuming a panic of its
    /// future), `None` once it was closed without an output for the handle.
    fn poll_task(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        match self.poll_raw(cx) {
            Poll::Ready(Some(Ok(output))) => Poll::Ready(Some(output)),
            Poll::Ready(Some(Err(panic))) => std::panic::resume_unwind(panic),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    /// The output once the task completed (or the panic of its future),
    /// `None` once it was closed without an output for the handle (cancelled,
    /// or dropped by its runtime) and its future is gone.
    fn poll_raw(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, Panic>>> {
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
                    return Poll::Ready(Some(self.read_output()));
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

    #[expect(
        clippy::panic,
        reason = "documented under `# Panics` of `Task`; `Task::fallible` answers `None` instead"
    )]
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

    /// The task's id.
    pub fn id(&self) -> super::Id {
        self.task.id()
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

/// The future of [`Task::join`]: the task's output, or why there is none.
#[must_use = "dropping it cancels the task, as dropping the `Task` does"]
pub struct Join<T> {
    task: Task<T>,
}

impl<T> Join<T> {
    /// Lets the task run to its end without a handle; see [`Task::detach`].
    pub fn detach(self) {
        self.task.detach()
    }

    /// The task's id.
    pub fn id(&self) -> super::Id {
        self.task.id()
    }
}

impl<T> Future for Join<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let id = self.task.id();
        self.task.poll_raw(cx).map(|output| match output {
            Some(Ok(output)) => Ok(output),
            Some(Err(panic)) => Err(JoinError {
                id,
                panic: Some(panic),
            }),
            None => Err(JoinError { id, panic: None }),
        })
    }
}

impl<T> fmt::Debug for Join<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Join").field(&self.task).finish()
    }
}

/// Why a task gave no output: it was cancelled (by its handle, an
/// [`AbortHandle`], or its runtime shutting down), or its future panicked.
pub struct JoinError {
    id: super::Id,
    panic: Option<Panic>,
}

impl JoinError {
    /// The task was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.panic.is_none()
    }

    /// The task's future panicked.
    pub fn is_panic(&self) -> bool {
        self.panic.is_some()
    }

    /// The payload of the panic.
    ///
    /// # Panics
    ///
    /// When the task was cancelled, not panicked; [`try_into_panic`](Self::try_into_panic)
    /// answers it instead.
    #[expect(clippy::panic, reason = "documented under `# Panics`")]
    pub fn into_panic(self) -> Box<dyn core::any::Any + Send + 'static> {
        match self.try_into_panic() {
            Ok(panic) => panic,
            Err(error) => panic!("`JoinError::into_panic` on a cancelled task ({error})"),
        }
    }

    /// The payload of the panic, or the error itself when the task was
    /// cancelled.
    pub fn try_into_panic(self) -> Result<Box<dyn core::any::Any + Send + 'static>, JoinError> {
        match self.panic {
            Some(panic) => Ok(panic),
            None => Err(self),
        }
    }

    /// The id of the task.
    pub fn id(&self) -> super::Id {
        self.id
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.panic {
            None => write!(f, "task {} was cancelled", self.id),
            Some(panic) => match panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
            {
                Some(message) => write!(f, "task {} panicked: {message}", self.id),
                None => write!(f, "task {} panicked", self.id),
            },
        }
    }
}

impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinError")
            .field("id", &self.id)
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl std::error::Error for JoinError {}

impl From<JoinError> for std::io::Error {
    fn from(error: JoinError) -> Self {
        std::io::Error::other(error.to_string())
    }
}

/// Cancels a task from anywhere: made by [`Task::abort_handle`], cloned and
/// sent freely, and it outlives the [`Task`]. It keeps the task's memory, not
/// its future: an aborted task's future is dropped where the task runs.
#[derive(Clone)]
pub struct AbortHandle {
    /// A reference of the task, as a waker of it holds one.
    waker: core::task::Waker,
    id: super::Id,
}

impl AbortHandle {
    /// Cancels the task unless it completed; see [`Task::abort`].
    pub fn abort(&self) {
        // SAFETY: the waker's reference keeps the task alive.
        unsafe { cancel(self.waker.data()) }
    }

    /// Whether the task completed or was cancelled.
    pub fn is_finished(&self) -> bool {
        // SAFETY: the waker's reference keeps the task alive.
        let header = unsafe { &*self.waker.data().cast::<Header>() };
        header.state.load(Ordering::Acquire) & (COMPLETED | CLOSED) != 0
    }

    /// The task's id.
    pub fn id(&self) -> super::Id {
        self.id
    }
}

impl fmt::Debug for AbortHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbortHandle").field("id", &self.id).finish()
    }
}
