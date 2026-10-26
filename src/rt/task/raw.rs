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
use super::state::{AWAITER, CLOSED, COMPLETED, FLAGS, REFERENCE, RUNNING, SCHEDULED, TASK};
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

    /// Allocates a task, scheduled, with its handle and its `Runnable`.
    pub(super) fn allocate(future: F, schedule: S) -> NonNull<()> {
        let cell = Box::new(Cell {
            header: Header::new(&Self::VTABLE),
            schedule,
            core: UnsafeCell::new(Core {
                future: ManuallyDrop::new(future),
            }),
        });
        // SAFETY: a box is never null.
        unsafe { NonNull::new_unchecked(Box::into_raw(cell).cast()) }
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
        // SAFETY: the caller owns the live future.
        unsafe {
            let cell = Self::cell(ptr);
            abort_on_panic(|| {
                cell.core
                    .with_mut(|core| ManuallyDrop::drop(&mut (*core).future))
            });
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
            abort_on_panic(|| drop(Box::from_raw(ptr.cast_mut().cast::<Cell<F, S>>())));
        }
    }

    /// Takes the awaiter, drops the run's reference, then wakes the awaiter:
    /// the end of a run that leaves the task completed or closed.
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

            let mut state = header.state.load(Ordering::Acquire);
            loop {
                if state & CLOSED != 0 {
                    // Cancelled while it waited in the queue.
                    Self::drop_future(ptr);
                    let state = header.state.fetch_and(!SCHEDULED, Ordering::AcqRel);
                    Self::finish(ptr, state);
                    return false;
                }
                match header.state.compare_exchange_weak(
                    state,
                    (state & !SCHEDULED) | RUNNING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => state = actual,
                }
            }

            let previous = super::enter(header.id);
            let poll = catch_unwind(AssertUnwindSafe(|| {
                cell.core
                    .with_mut(|core| F::poll(Pin::new_unchecked(&mut *(*core).future), cx))
            }));
            super::leave(previous);
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
                    Self::drop_future(ptr);
                    cell.core
                        .with_mut(|core| (*core).output = ManuallyDrop::new(output));
                    loop {
                        // With no handle left, nobody takes the output: the
                        // task closes and drops it.
                        let done = (state & !RUNNING & !SCHEDULED) | COMPLETED;
                        let new = if state & TASK == 0 {
                            done | CLOSED
                        } else {
                            done
                        };
                        match header.state.compare_exchange_weak(
                            state,
                            new,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                if state & TASK == 0 || state & CLOSED != 0 {
                                    abort_on_panic(|| {
                                        cell.core.with_mut(|core| {
                                            ManuallyDrop::drop(&mut (*core).output)
                                        })
                                    });
                                }
                                Self::finish(ptr, state);
                                return false;
                            }
                            Err(actual) => state = actual,
                        }
                    }
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
                        let new = if state & CLOSED != 0 {
                            state & !RUNNING & !SCHEDULED
                        } else {
                            state & !RUNNING
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
                                Self::drop_ref(ptr);
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
