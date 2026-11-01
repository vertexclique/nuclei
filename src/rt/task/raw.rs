//! A task's memory and every transition of its life, behind type-erased
//! function pointers.
//!
//! One allocation holds the [`Header`], the task's [`Schedule`] and a union
//! of its future and its output: the output is written where the future was
//! once the future is gone. The header comes first, so the task pointer is a
//! header pointer, and the [`TaskVTable`] it holds reaches the typed code
//! from a `Runnable`, a `Waker` or a `Task` that know only the pointer.
//!
//! # Invariants
//!
//! - **One poll at a time.** Only the holder of the `Runnable` polls, and it
//!   sets [`RUNNING`] first; a wake during the poll only sets [`SCHEDULED`],
//!   and the run reschedules the task after the poll.
//! - **The future is dropped once, by the side that owns it.** A `Runnable`
//!   (or the run) drops it, or the run that finds [`CLOSED`] after its poll,
//!   or the run whose reference turns out the last of a task with no handle
//!   (its last waker went during or just after the poll). A cancel, or the
//!   last waker, that finds the task idle schedules it once more instead, so
//!   the future of a task bound to one thread is dropped on that thread.
//! - **The memory lives while anything refers to it.** The count holds the
//!   `Runnable` and every `Waker`, the [`TASK`] flag the handle; the last of
//!   them to go frees the memory, after the future and the output are gone.
//! - **One waker for one task.** Every waker of a task points at the waker
//!   table inside its header's `TaskVTable`, so wakers of one task compare
//!   equal under `will_wake`, the poll's own included.

use core::future::Future;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::header::Header;
use super::state::{
    AWAITER, CLOSED, COMPLETED, FLAGS, NOTIFYING, REFERENCE, REGISTERING, RUNNING, SCHEDULED, TASK,
};
use super::{Runnable, Schedule, ScheduleInfo, abort_on_panic};
use crate::sync::internal::{Ordering, UnsafeCell};

/// What a poll that panicked left: the payload, handed to whoever awaits
/// the task.
pub(crate) type Panic = Box<dyn Any + Send + 'static>;

/// The typed functions of one kind of task, reached through its header.
pub(super) struct TaskVTable {
    /// Hands a `Runnable` the caller made for the task to its schedule.
    pub(super) schedule: unsafe fn(*const (), ScheduleInfo),
    /// Drops the future.
    pub(super) drop_future: unsafe fn(*const ()),
    /// Moves the output out, into the `Result<T, Panic>` the second pointer
    /// points to.
    pub(super) read_output: unsafe fn(*const (), *mut ()),
    /// Drops one reference.
    pub(super) drop_ref: unsafe fn(*const ()),
    /// Frees the memory.
    pub(super) destroy: unsafe fn(*const ()),
    /// Polls the future once; true when the task woke itself and was
    /// scheduled again.
    pub(super) run: unsafe fn(*const ()) -> bool,
    /// The table of every waker of the task.
    pub(super) waker: RawWakerVTable,
}

/// The memory of a task. `repr(C)` keeps the header at offset 0.
#[repr(C)]
struct Cell<F: Future, S> {
    header: Header,
    schedule: S,
    core: UnsafeCell<Core<F>>,
}

/// The future until it completes, then its output.
union Core<F: Future> {
    future: ManuallyDrop<F>,
    output: ManuallyDrop<Result<F::Output, Panic>>,
}

/// The typed functions of the tasks of future `F` and schedule `S`.
pub(super) struct RawTask<F, S>(PhantomData<(F, S)>);

impl<F, S> RawTask<F, S>
where
    F: Future,
    S: Schedule,
{
    const VTABLE: TaskVTable = TaskVTable {
        schedule: Self::schedule,
        drop_future: Self::drop_future,
        read_output: Self::read_output,
        drop_ref: Self::drop_ref,
        destroy: Self::destroy,
        run: Self::run,
        waker: RawWakerVTable::new(
            Self::clone_waker,
            Self::wake,
            Self::wake_by_ref,
            Self::drop_waker,
        ),
    };

    /// Allocates a task, scheduled, with its handle and its `Runnable`; its
    /// memory comes from the thread's cache of task blocks (see `memory`).
    #[inline(always)]
    pub(super) fn allocate(future: F, schedule: S) -> NonNull<()> {
        let (ptr, owner) = crate::mem::alloc(core::alloc::Layout::new::<Cell<F, S>>());
        let ptr = ptr.cast::<Cell<F, S>>();
        // SAFETY: fresh memory of the cell's layout, written once here.
        unsafe {
            ptr.as_ptr().write(Cell {
                header: Header::new(&Self::VTABLE, owner),
                schedule,
                core: UnsafeCell::new(Core {
                    future: ManuallyDrop::new(future),
                }),
            });
        }
        ptr.cast()
    }

    /// # Safety
    ///
    /// `ptr` is a live task of this kind.
    unsafe fn cell<'a>(ptr: *const ()) -> &'a Cell<F, S> {
        // SAFETY: the caller's contract.
        unsafe { &*ptr.cast::<Cell<F, S>>() }
    }

    unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
        // SAFETY: a waker refers to a live task.
        let header = unsafe { &Self::cell(ptr).header };
        // Relaxed, as for any reference count: the clone holds a reference
        // already, so nothing it reads is new.
        let state = header.state.fetch_add(REFERENCE, Ordering::Relaxed);
        if state > isize::MAX as usize {
            std::process::abort();
        }
        RawWaker::new(ptr, &header.vtable.waker)
    }

    unsafe fn wake(ptr: *const ()) {
        // SAFETY: a waker refers to a live task, and this one's reference is
        // spent below exactly once.
        unsafe {
            let header = &Self::cell(ptr).header;
            let mut state = header.state.load(Ordering::Acquire);
            loop {
                if state & (COMPLETED | CLOSED) != 0 {
                    Self::drop_waker(ptr);
                    return;
                }
                let new = state | SCHEDULED;
                // Already scheduled, the exchange changes nothing but still
                // publishes what the waker wrote before to the run.
                match header.state.compare_exchange_weak(
                    state,
                    new,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) if state & (SCHEDULED | RUNNING) == 0 => {
                        // The waker's reference becomes the runnable's.
                        Self::schedule(ptr, ScheduleInfo::new(false));
                        return;
                    }
                    // Already scheduled, or running: the run reschedules it.
                    Ok(_) => {
                        Self::drop_waker(ptr);
                        return;
                    }
                    Err(actual) => state = actual,
                }
            }
        }
    }

    unsafe fn wake_by_ref(ptr: *const ()) {
        // SAFETY: a waker refers to a live task, which it keeps alive while
        // the schedule runs.
        unsafe {
            let header = &Self::cell(ptr).header;
            let mut state = header.state.load(Ordering::Acquire);
            loop {
                if state & (COMPLETED | CLOSED) != 0 {
                    return;
                }
                let idle = state & (SCHEDULED | RUNNING) == 0;
                // An idle task gets a reference for its new runnable.
                let new = if idle {
                    (state | SCHEDULED) + REFERENCE
                } else {
                    state | SCHEDULED
                };
                match header.state.compare_exchange_weak(
                    state,
                    new,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        if idle {
                            if state > isize::MAX as usize {
                                std::process::abort();
                            }
                            Self::schedule(ptr, ScheduleInfo::new(false));
                        }
                        return;
                    }
                    Err(actual) => state = actual,
                }
            }
        }
    }

    unsafe fn drop_waker(ptr: *const ()) {
        // SAFETY: a waker refers to a live task; its reference goes here.
        unsafe {
            let header = &Self::cell(ptr).header;
            let new = header.state.fetch_sub(REFERENCE, Ordering::AcqRel) - REFERENCE;
            if new & !FLAGS == 0 && new & TASK == 0 {
                if new & (COMPLETED | CLOSED) == 0 {
                    // Nothing can wake the task any more and nobody awaits it:
                    // it is closed and scheduled once more, so its future is
                    // dropped where it runs. No reference is left to race.
                    header
                        .state
                        .store(SCHEDULED | CLOSED | REFERENCE, Ordering::Release);
                    Self::schedule(ptr, ScheduleInfo::new(false));
                } else {
                    Self::destroy(ptr);
                }
            }
        }
    }

    /// Drops a reference held by a run, or by a `Runnable` that closed the
    /// task; the last reference with no handle frees the task.
    ///
    /// Called on a runner whenever the task is still open: a run that ends
    /// Pending and finds its reference the last one, with no handle, holds a
    /// task nothing can wake or await again (its last waker went during the
    /// poll, or just after it). The future is dropped here, on the runner,
    /// rather than left behind when the memory goes.
    unsafe fn drop_ref(ptr: *const ()) {
        // SAFETY: the caller holds the reference it drops; with none left and
        // no handle, nobody else reaches the task.
        unsafe {
            let header = &Self::cell(ptr).header;
            let new = header.state.fetch_sub(REFERENCE, Ordering::AcqRel) - REFERENCE;
            if new & !FLAGS == 0 && new & TASK == 0 {
                if new & (COMPLETED | CLOSED) == 0 {
                    Self::drop_future(ptr);
                }
                Self::destroy(ptr);
            }
        }
    }

    /// Hands a `Runnable` for the task to its schedule. The caller made the
    /// reference the runnable takes over.
    unsafe fn schedule(ptr: *const (), info: ScheduleInfo) {
        // SAFETY: the reference handed over keeps the task alive until the
        // runnable is run or dropped; the schedule's own contract covers
        // what it does after a handoff to another thread.
        unsafe {
            let runnable = Runnable::from_raw(NonNull::new_unchecked(ptr.cast_mut()));
            Self::cell(ptr).schedule.schedule(runnable, info);
        }
    }

    unsafe fn drop_future(ptr: *const ()) {
        // SAFETY: the caller's contract.
        unsafe { Self::drop_future_here(ptr) }
    }

    /// [`drop_future`](Self::drop_future), inline where a run completes.
    ///
    /// # Safety
    ///
    /// The caller owns the live future.
    #[inline(always)]
    unsafe fn drop_future_here(ptr: *const ()) {
        // SAFETY: the caller owns the live future.
        unsafe {
            let cell = Self::cell(ptr);
            cell.schedule.drop_future(|| {
                abort_on_panic(|| {
                    cell.core
                        .with_mut(|core| ManuallyDrop::drop(&mut (*core).future))
                })
            });
            cell.schedule.terminated(cell.header.id);
        }
    }

    unsafe fn read_output(ptr: *const (), out: *mut ()) {
        // SAFETY: the caller closed the completed task, so the output is
        // there and it alone takes it.
        unsafe {
            let cell = Self::cell(ptr);
            cell.core.with_mut(|core| {
                out.cast::<Result<F::Output, Panic>>()
                    .write(ManuallyDrop::take(&mut (*core).output));
            });
        }
    }

    unsafe fn destroy(ptr: *const ()) {
        // SAFETY: nothing refers to the task any more; its future and output
        // are gone, and the union has no drop glue of its own.
        unsafe {
            let header = &Self::cell(ptr).header;
            super::names::forget(header.id);
            let owner = header.owner;
            let cell = ptr.cast_mut().cast::<Cell<F, S>>();
            abort_on_panic(|| core::ptr::drop_in_place(cell));
            // SAFETY: the memory came from `mem::alloc` with the cell's
            // layout and `owner`, and nothing uses it now.
            crate::mem::free(
                NonNull::new_unchecked(cell.cast()),
                core::alloc::Layout::new::<Cell<F, S>>(),
                owner,
            );
        }
    }

    /// Takes the awaiter, drops the run's reference, then wakes the awaiter:
    /// the end of a run that leaves the task completed or closed.
    #[inline(always)]
    unsafe fn finish(ptr: *const (), state: usize) {
        // SAFETY: the run's reference is live until `drop_ref`, and the
        // awaiter taken first does not need the task.
        unsafe {
            let header = &Self::cell(ptr).header;
            let awaiter = if state & AWAITER != 0 {
                header.take(None)
            } else {
                None
            };
            Self::drop_ref(ptr);
            if let Some(waker) = awaiter {
                abort_on_panic(|| waker.wake());
            }
        }
    }

    unsafe fn run(ptr: *const ()) -> bool {
        // SAFETY: the runnable's reference is the run's: the task is live
        // and only this thread polls it until RUNNING drops.
        unsafe {
            let cell = Self::cell(ptr);
            let header = &cell.header;
            // The poll's waker borrows the run's reference.
            let waker =
                ManuallyDrop::new(Waker::from_raw(RawWaker::new(ptr, &header.vtable.waker)));
            let cx = &mut Context::from_waker(&waker);

            // A runnable's task is scheduled and not running, and nothing
            // clears SCHEDULED while the runnable waits: one addition turns
            // SCHEDULED into RUNNING, whatever else the word holds.
            let state = header
                .state
                .fetch_add(RUNNING - SCHEDULED, Ordering::AcqRel);
            debug_assert_eq!(state & (SCHEDULED | RUNNING), SCHEDULED);
            if state & CLOSED != 0 {
                // Cancelled while it waited in the queue.
                Self::drop_future(ptr);
                let state = header.state.fetch_and(!RUNNING, Ordering::AcqRel);
                Self::finish(ptr, state);
                return false;
            }

            // The task and a fresh budget are this thread's for the poll, the
            // ones before them put back after it (a `block_on` inside a task
            // polls other tasks in between).
            let entered = crate::coop::enter_task(header.id);
            // Each schedule's hooks are a check of its own, inline.
            cell.schedule.before_poll(header.id);
            let poll = catch_unwind(AssertUnwindSafe(|| {
                cell.core
                    .with_mut(|core| F::poll(Pin::new_unchecked(&mut *(*core).future), cx))
            }));
            cell.schedule.after_poll(header.id);
            crate::coop::leave_task(entered);
            // The poll may have woken the task, cloned its waker, or a handle
            // may have registered meanwhile: the word is read again rather
            // than fed stale into the exchanges below, which would fail once.
            let mut state = header.state.load(Ordering::Acquire);

            let output = match poll {
                Ok(Poll::Pending) => None,
                Ok(Poll::Ready(value)) => Some(Ok(value)),
                Err(payload) => Some(Err(payload)),
            };
            match output {
                Some(output) => {
                    let panicked = output.is_err();
                    Self::drop_future_here(ptr);
                    cell.core
                        .with_mut(|core| (*core).output = ManuallyDrop::new(output));
                    // Before the exchange below, which may hand the memory to
                    // the handle: the hook, and the awaiter, taken with the
                    // notifier's flag held across the exchange (a registration
                    // that meets it wakes its own waker).
                    if panicked {
                        cell.schedule.panicked(header.id);
                    }
                    let mut awaiter = None;
                    let mut claimed = false;
                    let mut unclaimed = None;
                    let last = loop {
                        // A waker stored, its cell free, and not the run's yet:
                        // claimed first, so the exchange clears its flags. One a
                        // registration or a notification holds is theirs to wake.
                        if !claimed
                            && state & AWAITER != 0
                            && state & (NOTIFYING | REGISTERING) == 0
                        {
                            let (waker, ours, now) = header.claim();
                            if ours {
                                awaiter = waker;
                                claimed = true;
                            }
                            state = now;
                        }
                        // With no handle left, or a handle that cancelled, nobody
                        // takes the output: the run takes it out, while its
                        // reference still holds the task, and drops it once the
                        // task is let go of (a detached blocking job's end so
                        // follows its free: `fs::call::Ending`).
                        if (state & TASK == 0 || state & CLOSED != 0) && unclaimed.is_none() {
                            unclaimed = Some(
                                cell.core
                                    .with_mut(|core| ManuallyDrop::take(&mut (*core).output)),
                            );
                        }
                        // Completed, closed too with no handle, and the run's
                        // reference gone in the same exchange: a handle that
                        // sees the completion holds the last reference, so its
                        // drop frees the task where the handle is.
                        let mut new = (state & !RUNNING & !SCHEDULED) | COMPLETED;
                        if state & TASK == 0 {
                            new |= CLOSED;
                        }
                        if claimed {
                            new &= !(NOTIFYING | AWAITER);
                        }
                        new -= REFERENCE;
                        match header.state.compare_exchange_weak(
                            state,
                            new,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => break new & !FLAGS == 0 && new & TASK == 0,
                            Err(actual) => state = actual,
                        }
                    };
                    // Nothing of the task is touched from here on, unless the
                    // run's reference was the last one, with no handle.
                    if last {
                        Self::destroy(ptr);
                    }
                    // Before the awaiter learns of the end: what a cancel waits
                    // for is gone by then.
                    if let Some(output) = unclaimed {
                        abort_on_panic(|| drop(output));
                    }
                    if let Some(waker) = awaiter {
                        abort_on_panic(|| waker.wake());
                    }
                    false
                }
                None => {
                    let mut future_dropped = false;
                    loop {
                        if state & CLOSED != 0 && !future_dropped {
                            // Cancelled during the poll: the canceller left
                            // the future to this run.
                            Self::drop_future(ptr);
                            future_dropped = true;
                        }
                        // Neither woken nor closed: the run's reference goes in
                        // the same exchange, so no later run of the task (woken
                        // meanwhile on another thread) is followed by this one's
                        // drop.
                        let new = if state & CLOSED != 0 {
                            state & !RUNNING & !SCHEDULED
                        } else if state & SCHEDULED != 0 {
                            state & !RUNNING
                        } else {
                            (state & !RUNNING) - REFERENCE
                        };
                        match header.state.compare_exchange_weak(
                            state,
                            new,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) if state & CLOSED != 0 => {
                                Self::finish(ptr, state);
                                return false;
                            }
                            Ok(_) if state & SCHEDULED != 0 => {
                                // Woken during the poll: the run's reference
                                // goes to the new runnable.
                                Self::schedule(ptr, ScheduleInfo::new(true));
                                return true;
                            }
                            Ok(_) => {
                                // The last reference, with no handle: nothing
                                // can wake or await the task again.
                                if new & !FLAGS == 0 && new & TASK == 0 {
                                    Self::drop_future(ptr);
                                    Self::destroy(ptr);
                                }
                                return false;
                            }
                            Err(actual) => state = actual,
                        }
                    }
                }
            }
        }
    }
}
