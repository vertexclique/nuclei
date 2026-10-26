//! Tasks: a spawned future, the [`Runnable`] a run queue holds for it, and
//! the [`Task`] handle that awaits its output.
//!
//! A task is one allocation (see `raw`) with one state word. Spawning
//! returns its `Runnable`, already scheduled, and its handle. Running the
//! runnable polls the future once; a waker that fires makes a new runnable
//! and hands it to the task's [`Schedule`], which puts it in a run queue. A
//! wake during the poll only marks the task, and the run hands it to the
//! schedule once the poll is over, so a task is never polled twice at once
//! and never re-polled from inside its own poll.
//!
//! The semantics are those of the established task crate the old executor
//! was built on:
//!
//! - A poll that panics completes the task with the panic, and awaiting the
//!   handle resumes it; a detached task's panic is dropped (the panic hook
//!   has already reported it).
//! - Dropping the handle cancels the task: its future is dropped by the next
//!   run, or right away by the runnable's drop. [`Task::detach`] lets it run
//!   to the end instead, and [`Task::cancel`] cancels and returns the output
//!   if the task had completed.
//! - Dropping a runnable without running it cancels the task and drops its
//!   future there; awaiting the handle then panics, and a
//!   [`FallibleTask`] returns `None`.
//!
//! Two fixes the established crate has not released are part of this one:
//! the poll's waker is the task's canonical waker (`will_wake` holds between
//! it and its clones), and the state is read again after the poll, so the
//! exchange after a poll that woke the task or cloned its waker does not
//! start from a value it is sure to miss.

mod header;
mod join;
#[cfg(all(test, loom))]
mod loom_tests;
pub(crate) mod names;
mod raw;
mod state;
#[cfg(all(test, not(loom)))]
mod tests;

use core::future::Future;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::task::{RawWaker, Waker};

use self::header::Header;
pub use self::join::{AbortHandle, FallibleTask, Join, JoinError, Task};

/// A task's id: unique among the tasks of the process, never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub(super) core::num::NonZeroU64);

impl core::fmt::Display for Id {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

std::thread_local! {
    /// The id of the task this thread is polling, if any.
    static CURRENT: core::cell::Cell<Option<Id>> = const { core::cell::Cell::new(None) };
}

/// The id of the task being polled on this thread, `None` outside a task (in
/// `block_on`'s own future, on a blocking-pool thread).
pub(crate) fn current_id() -> Option<Id> {
    CURRENT.try_with(core::cell::Cell::get).ok().flatten()
}

/// Sets the current task while polling it, and answers the one before it
/// (a `block_on` inside a task polls other tasks in between).
pub(super) fn enter(id: Id) -> Option<Id> {
    CURRENT.try_with(|c| c.replace(Some(id))).ok().flatten()
}

/// Puts back the task `enter` replaced.
pub(super) fn leave(previous: Option<Id>) {
    let _ = CURRENT.try_with(|c| c.set(previous));
}
use self::raw::RawTask;

/// How a task came to be scheduled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScheduleInfo {
    /// The task woke itself during its own poll (a yield, an IO operation
    /// that completed at once): a run queue gives other tasks a turn first.
    pub(crate) woken_while_running: bool,
}

impl ScheduleInfo {
    pub(crate) const fn new(woken_while_running: bool) -> Self {
        Self {
            woken_while_running,
        }
    }
}

/// Where a woken task goes.
///
/// # Safety
///
/// `schedule` is called with `self` borrowed from the task's own memory.
/// Once it has handed the runnable to a place another thread takes it from,
/// that thread may run the task to its end and free it, `self` included: an
/// implementation then must not touch `self` again unless it took a
/// reference to the task before the handoff (a [`Runnable::waker`]).
pub(crate) unsafe trait Schedule {
    fn schedule(&self, runnable: Runnable, info: ScheduleInfo);

    /// Just before each poll of the task's future.
    fn before_poll(&self, _id: Id) {}

    /// Just after each poll of the task's future.
    fn after_poll(&self, _id: Id) {}

    /// The task's future was dropped: it completed or was cancelled.
    fn terminated(&self, _id: Id) {}

    /// A poll of the task's future panicked, its outcome now.
    fn panicked(&self, _id: Id) {}
}

/// A scheduled task: running it polls the task's future once.
///
/// Dropping a runnable without running it cancels the task and drops its
/// future on the dropping thread.
pub(crate) struct Runnable {
    ptr: NonNull<()>,
}

// SAFETY: a runnable is a reference to a task whose future, for a `spawn`ed
// task, is `Send`; for a task of `spawn_unchecked` the caller promised that
// the runnable is run and dropped only where that is sound.
unsafe impl Send for Runnable {}
// SAFETY: `&Runnable` only reads the header or clones a waker.
unsafe impl Sync for Runnable {}

impl Runnable {
    /// # Safety
    ///
    /// `ptr` is a task pointer whose reference the runnable takes over.
    pub(crate) unsafe fn from_raw(ptr: NonNull<()>) -> Self {
        Self { ptr }
    }

    /// The task pointer, keeping the reference: [`from_raw`](Self::from_raw)
    /// makes the runnable again.
    pub(crate) fn into_raw(self) -> NonNull<()> {
        ManuallyDrop::new(self).ptr
    }

    fn header(&self) -> &Header {
        // SAFETY: the runnable's reference keeps the task alive.
        unsafe { self.ptr.cast::<Header>().as_ref() }
    }

    /// Polls the task's future once. Returns true when the task woke itself
    /// during the poll and was handed to its schedule again.
    pub(crate) fn run(self) -> bool {
        let ptr = self.into_raw().as_ptr();
        // SAFETY: the runnable's reference goes to the run.
        unsafe { ((*ptr.cast::<Header>()).vtable.run)(ptr) }
    }

    /// Hands the runnable back to the task's schedule.
    pub(crate) fn schedule(self) {
        let ptr = self.into_raw().as_ptr();
        // SAFETY: the runnable's reference goes to the new runnable.
        unsafe { ((*ptr.cast::<Header>()).vtable.schedule)(ptr, ScheduleInfo::new(false)) }
    }

    /// A waker of the task, which holds its own reference.
    pub(crate) fn waker(&self) -> Waker {
        let ptr = self.ptr.as_ptr().cast_const();
        // SAFETY: the borrowed waker is never dropped, so it counts no
        // reference; its clone counts its own.
        let borrowed = ManuallyDrop::new(unsafe {
            Waker::from_raw(RawWaker::new(ptr, &self.header().vtable.waker))
        });
        (*borrowed).clone()
    }
}

impl Drop for Runnable {
    fn drop(&mut self) {
        use self::state::{AWAITER, CLOSED, COMPLETED, SCHEDULED};
        let ptr = self.ptr.as_ptr().cast_const();
        let header = self.header();
        let mut state = header.state.load(crate::sync::internal::Ordering::Acquire);
        loop {
            if state & (COMPLETED | CLOSED) != 0 {
                break;
            }
            match header.state.compare_exchange_weak(
                state,
                state | CLOSED,
                crate::sync::internal::Ordering::AcqRel,
                crate::sync::internal::Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => state = actual,
            }
        }
        // SAFETY: a runnable exists only for a task that has not completed,
        // so its future is there, and the runnable owns it now.
        unsafe { (header.vtable.drop_future)(ptr) };
        let state = header
            .state
            .fetch_and(!SCHEDULED, crate::sync::internal::Ordering::AcqRel);
        if state & AWAITER != 0 {
            header.notify(None);
        }
        // SAFETY: the runnable's reference.
        unsafe { (header.vtable.drop_ref)(ptr) };
    }
}

impl core::fmt::Debug for Runnable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Runnable").field("task", &self.ptr).finish()
    }
}

/// Spawns `future` as a task that `schedule` places when it is woken.
/// Returns the task's runnable, scheduled once and not yet handed to
/// `schedule`, and its handle.
pub(crate) fn spawn<F, S>(future: F, schedule: S) -> (Runnable, Task<F::Output>)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
    S: Schedule + Send + Sync + 'static,
{
    // SAFETY: everything the task holds may cross threads and lives as long
    // as it needs to.
    unsafe { spawn_unchecked(future, schedule) }
}

/// [`spawn`] without the `Send` and `'static` bounds.
///
/// # Safety
///
/// The future is polled and dropped only on a thread where that is sound
/// (its owner's, for a future that is not `Send`), everything it borrows
/// outlives the task, and the schedule may be called from any thread that
/// holds a waker.
pub(crate) unsafe fn spawn_unchecked<F, S>(future: F, schedule: S) -> (Runnable, Task<F::Output>)
where
    F: Future,
    S: Schedule,
{
    let ptr = RawTask::<F, S>::allocate(future, schedule);
    // SAFETY: a new task holds the runnable's reference and the handle's flag.
    unsafe { (Runnable::from_raw(ptr), Task::from_raw(ptr)) }
}

/// Runs `f`, aborting the process if it panics: what a destructor or a waker
/// that panics inside a task transition gets, as unwinding there would leave
/// the state word half done.
#[inline]
pub(crate) fn abort_on_panic<R>(f: impl FnOnce() -> R) -> R {
    struct Abort;
    impl Drop for Abort {
        fn drop(&mut self) {
            std::process::abort();
        }
    }
    let abort = Abort;
    let out = f();
    core::mem::forget(abort);
    out
}
