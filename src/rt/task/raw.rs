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
//! - **No transition unwinds.** The task's own code that runs outside its
//!   poll's catch (its future's drop, a hook, an output nobody takes, the
//!   drop of what the memory holds) has its panic caught where it happens.
//!   A panic of the poll, a hook or the future's drop is the task's outcome
//!   for its handle, even a handle that cancelled: a cancel whose future's
//!   drop panicked ends as a completion with that panic, closed no more
//!   ([`settle`](RawTask::settle)). With no handle left, a panic is logged
//!   at error (`proofs/tla/task/Task.tla`).
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
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::header::Header;
use super::header::wake_caught;
use super::state::{
    AWAITER, CLOSED, COMPLETED, FLAGS, NOTIFYING, REFERENCE, REGISTERING, RUNNING, SCHEDULED, TASK,
};
use super::{Id, Runnable, Schedule, ScheduleInfo};
pub(crate) use crate::rt::caught::Panic;
use crate::rt::caught::{self, Thin};
use crate::sync::internal::{Ordering, UnsafeCell};

/// The typed functions of one kind of task, reached through its header.
pub(super) struct TaskVTable {
    /// Hands a `Runnable` the caller made for the task to its schedule.
    pub(super) schedule: unsafe fn(*const (), ScheduleInfo),
    /// Drops a runnable that is not run: the task closes, its future goes.
    pub(super) drop_runnable: unsafe fn(*const ()),
    /// Moves the output out, into the `Result<T, Panic>` the second pointer
    /// points to.
    pub(super) read_output: unsafe fn(*const (), *mut ()),
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
        drop_runnable: Self::drop_runnable,
        read_output: Self::read_output,
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
                    Self::end_unawaited(ptr);
                } else {
                    Self::destroy(ptr);
                }
            }
        }
    }

    /// Ends an open task whose last reference was just dropped, with no handle: closed, with a
    /// reference of its own while its future is dropped, so one a list edit posted in the drop
    /// takes keeps the task (its last drop finds it closed and frees it), and the future is not
    /// dropped twice. The task is freed here only if that reference is the last.
    ///
    /// # Safety
    ///
    /// The task is open with no reference left and no handle: nothing else touches it.
    #[cold]
    #[inline(never)]
    unsafe fn end_unawaited(ptr: *const ()) {
        // SAFETY: the caller's contract; the reference added here keeps the task meanwhile.
        unsafe {
            let header = &Self::cell(ptr).header;
            header.state.fetch_add(REFERENCE + CLOSED, Ordering::AcqRel);
            Self::drop_unawaited(ptr);
            let new = header.state.fetch_sub(REFERENCE, Ordering::AcqRel) - REFERENCE;
            if new & !FLAGS == 0 && new & TASK == 0 {
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

    /// [`drop_future_here`](Self::drop_future_here) out of line, for a cancel.
    ///
    /// # Safety
    ///
    /// The caller owns the live future.
    unsafe fn drop_future(ptr: *const ()) -> Option<Panic> {
        // SAFETY: the caller's contract.
        unsafe { Self::drop_future_here(ptr) }
    }

    /// Drops the future of a task nobody can wake or await any more: a
    /// panic of the drop is logged.
    ///
    /// # Safety
    ///
    /// The caller owns the live future.
    unsafe fn drop_unawaited(ptr: *const ()) {
        // SAFETY: the caller's contract, and its reference keeps the task.
        unsafe {
            if let Some(panic) = Self::drop_future(ptr) {
                lost_ending(Self::cell(ptr).header.id, panic);
            }
        }
    }

    /// Drops the future, then tells the schedule; inline where a run
    /// completes. A panic of the drop, or of the terminate hook after it, is
    /// caught and returned: the drop's, the hook's then logged.
    ///
    /// # Safety
    ///
    /// The caller owns the live future.
    #[inline(always)]
    unsafe fn drop_future_here(ptr: *const ()) -> Option<Panic> {
        // SAFETY: the caller owns the live future.
        unsafe { Self::drop_future_in::<false>(ptr) }
    }

    /// [`drop_future_here`](Self::drop_future_here), through the schedule's way for a thread that
    /// may not own the task's list when `OFF`.
    ///
    /// # Safety
    ///
    /// The caller owns the live future.
    #[inline(always)]
    unsafe fn drop_future_in<const OFF: bool>(ptr: *const ()) -> Option<Panic> {
        // SAFETY: the caller owns the live future.
        unsafe {
            let cell = Self::cell(ptr);
            let mut dropped = None;
            macro_rules! dropping {
                () => {
                    #[inline(always)]
                    || {
                        dropped = caught::catch(|| {
                            cell.core
                                .with_mut(|core| ManuallyDrop::drop(&mut (*core).future))
                        });
                    }
                };
            }
            if OFF {
                cell.schedule
                    .drop_future_off(&cell.header.links, dropping!());
            } else {
                cell.schedule.drop_future(&cell.header.links, dropping!());
            }
            if dropped.is_none() && !cell.schedule.has_hooks() {
                return None;
            }
            ended_with(&cell.schedule, cell.header.id, dropped)
        }
    }

    /// [`drop_future`](Self::drop_future) for a runnable dropped on any thread.
    ///
    /// # Safety
    ///
    /// The caller owns the live future.
    #[cold]
    #[inline(never)]
    unsafe fn drop_future_off(ptr: *const ()) -> Option<Panic> {
        // SAFETY: the caller's contract.
        unsafe { Self::drop_future_in::<true>(ptr) }
    }

    /// The exchange that ends a cancel once the future is gone: clears
    /// `clear` from the word and returns the word before. `panic`: the
    /// future's drop panicked, and [`settle_panicked`](Self::settle_panicked)
    /// hands the panic on.
    ///
    /// # Safety
    ///
    /// The caller holds a reference and dropped the future.
    #[inline(always)]
    unsafe fn settle(ptr: *const (), clear: usize, panic: Option<Panic>) -> usize {
        // SAFETY: the caller's contract.
        unsafe {
            match panic {
                None => Self::cell(ptr)
                    .header
                    .state
                    .fetch_and(!clear, Ordering::AcqRel),
                Some(panic) => Self::settle_panicked(ptr, clear, panic),
            }
        }
    }

    /// [`settle`](Self::settle) after a drop that panicked: the one exchange
    /// also completes the task with the panic, closed no more, while its
    /// handle is there, so whoever awaits the handle resumes it. With no
    /// handle left the panic is logged.
    ///
    /// # Safety
    ///
    /// As [`settle`](Self::settle).
    #[cold]
    #[inline(never)]
    unsafe fn settle_panicked(ptr: *const (), clear: usize, panic: Panic) -> usize {
        // SAFETY: the caller's reference keeps the task. The future is gone,
        // and nothing reads the cell until the exchange says it holds an
        // output, or after the exchange if it does not.
        unsafe {
            let cell = Self::cell(ptr);
            let header = &cell.header;
            cell.core
                .with_mut(|core| (*core).output = ManuallyDrop::new(Err(panic)));
            let mut state = header.state.load(Ordering::Acquire);
            loop {
                let new = if state & TASK != 0 {
                    (state & !clear & !CLOSED) | COMPLETED
                } else {
                    state & !clear
                };
                match header.state.compare_exchange_weak(
                    state,
                    new,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => state = actual,
                }
            }
            if state & TASK == 0
                && let Err(panic) = cell
                    .core
                    .with_mut(|core| ManuallyDrop::take(&mut (*core).output))
            {
                lost_ending(header.id, panic);
            }
            state
        }
    }

    /// A runnable dropped without running (`Runnable`'s drop): the task
    /// closes unless it completed, the future the runnable owns goes, and so
    /// does the runnable's reference.
    ///
    /// # Safety
    ///
    /// `ptr` is the task of the runnable, whose reference this takes.
    unsafe fn drop_runnable(ptr: *const ()) {
        // SAFETY: the runnable's reference keeps the task until `drop_ref`.
        unsafe {
            let header = &Self::cell(ptr).header;
            let mut state = header.state.load(Ordering::Acquire);
            loop {
                if state & (COMPLETED | CLOSED) != 0 {
                    break;
                }
                match header.state.compare_exchange_weak(
                    state,
                    state | CLOSED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => state = actual,
                }
            }
            // A runnable exists only for a task that has not completed, so
            // its future is there, and the runnable owns it now.
            let panic = Self::drop_future_off(ptr);
            let state = Self::settle(ptr, SCHEDULED, panic);
            if state & AWAITER != 0 {
                header.notify(None);
            }
            Self::drop_ref(ptr);
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
            // What the memory holds (the schedule, a stored waker) may panic
            // as it goes: the memory is freed all the same.
            if let Some(panic) = caught::catch(|| core::ptr::drop_in_place(cell)) {
                lost_freeing(panic);
            }
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
                wake_caught(waker);
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
                return Self::end_queued(ptr);
            }

            // The task and a fresh budget are this thread's for the poll, the
            // ones before them put back after it (a `block_on` inside a task
            // polls other tasks in between).
            let entered = crate::coop::enter_task(header.id);
            // Each schedule's hooks are a check of its own, inline; a hook
            // that panics panics the poll.
            let poll = catch_unwind(AssertUnwindSafe(|| {
                cell.schedule.before_poll(cell.header.id);
                cell.core
                    .with_mut(|core| F::poll(Pin::new_unchecked(&mut *(*core).future), cx))
            }))
            .map_err(Box::new);
            if cell.schedule.has_hooks() {
                return Self::run_hooked(ptr, &entered, poll);
            }
            Self::settle_run(ptr, &entered, poll)
        }
    }

    /// The end of a run whose schedule has an after-poll hook: out of line,
    /// so the normal path tests one flag.
    ///
    /// # Safety
    ///
    /// As [`settle_run`](Self::settle_run).
    #[cold]
    #[inline(never)]
    unsafe fn run_hooked(
        ptr: *const (),
        entered: &crate::coop::Entered,
        poll: Result<Poll<F::Output>, Thin>,
    ) -> bool {
        // SAFETY: the caller's contract.
        unsafe {
            let poll =
                after_poll_hooked(&Self::cell(ptr).schedule, Self::cell(ptr).header.id, poll);
            Self::settle_run(ptr, entered, poll)
        }
    }

    /// What follows a poll: leaves the task, then the outcome or the next
    /// state.
    ///
    /// # Safety
    ///
    /// The run's reference, RUNNING set, and `poll` the result of this
    /// run's poll.
    #[inline(always)]
    unsafe fn settle_run(
        ptr: *const (),
        entered: &crate::coop::Entered,
        poll: Result<Poll<F::Output>, Thin>,
    ) -> bool {
        // SAFETY: the caller's contract.
        unsafe {
            let cell = Self::cell(ptr);
            let header = &cell.header;
            crate::coop::leave_task(*entered);
            // The poll may have woken the task, cloned its waker, or a handle
            // may have registered meanwhile: the word is read again rather
            // than fed stale into the exchanges below, which would fail once.
            let mut state = header.state.load(Ordering::Acquire);

            let output = match poll {
                Ok(Poll::Pending) => None,
                Ok(Poll::Ready(value)) => Some(Ok(value)),
                Err(payload) => Some(Err(*payload)),
            };
            match output {
                Some(mut output) => {
                    if let Some(panic) = Self::drop_future_here(ptr) {
                        output = outcome_with(header.id, output, panic);
                    }
                    let panicked = output.is_err();
                    cell.core
                        .with_mut(|core| (*core).output = ManuallyDrop::new(output));
                    // Before the exchange, which may hand the memory to the
                    // handle.
                    if panicked {
                        cell.schedule.panicked(header.id);
                        return Self::complete_panicked(ptr, state);
                    }
                    Self::complete::<false>(ptr, state)
                }
                None => {
                    cell.schedule.pending(&header.links);
                    loop {
                        if state & CLOSED != 0 {
                            return Self::end_closed(ptr);
                        }
                        // Neither woken nor closed: the run's reference goes in
                        // the same exchange, so no later run of the task (woken
                        // meanwhile on another thread) is followed by this one's
                        // drop.
                        let new = if state & SCHEDULED != 0 {
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
                            Ok(_) if state & SCHEDULED != 0 => {
                                // Woken during the poll: the run's reference goes
                                // to the new runnable.
                                Self::schedule(ptr, ScheduleInfo::new(true));
                                return true;
                            }
                            Ok(_) => {
                                // The last reference, with no handle: nothing can
                                // wake or await the task again.
                                if new & !FLAGS == 0 && new & TASK == 0 {
                                    Self::end_unawaited(ptr);
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

    /// The end of a run whose poll gave the task its outcome, in the cell:
    /// the awaiter claimed, then one exchange completes the task and drops
    /// the run's reference; then the output nobody takes is dropped and the
    /// awaiter woken. `PANICKED`: the outcome is a panic, which stays for a
    /// handle, even one that cancelled (closed no more), and is taken out
    /// and logged only with no handle left. A value stays for a handle that
    /// did not cancel.
    ///
    /// # Safety
    ///
    /// The run's reference, and the outcome written in the cell.
    #[inline(always)]
    unsafe fn complete<const PANICKED: bool>(ptr: *const (), mut state: usize) -> bool {
        // SAFETY: the run's reference is live until the exchange drops it,
        // and nothing of the task is touched after it unless it was the last.
        unsafe {
            let cell = Self::cell(ptr);
            let header = &cell.header;
            // The hook, and the awaiter, taken with the notifier's flag held
            // across the exchange (a registration that meets it wakes its own
            // waker).
            let mut awaiter = None;
            let mut claimed = false;
            let mut unclaimed = None;
            let last = loop {
                // A waker stored, its cell free, and not the run's yet:
                // claimed first, so the exchange clears its flags. One a
                // registration or a notification holds is theirs to wake.
                if !claimed && state & AWAITER != 0 && state & (NOTIFYING | REGISTERING) == 0 {
                    let (waker, ours, now) = header.claim();
                    if ours {
                        awaiter = waker;
                        claimed = true;
                    }
                    state = now;
                }
                // With no handle left, or a value for a handle that cancelled,
                // nobody takes the output: the run takes it out, while its
                // reference still holds the task. A handle that cancelled
                // answers `None` once it sees the task closed and not running,
                // so the output is dropped here, before the exchange that
                // clears RUNNING; with no handle it is dropped once the task
                // is let go of (a detached blocking job's end so follows its
                // free: `fs::call::Ending`).
                if unclaimed.is_none() {
                    if state & TASK == 0 {
                        unclaimed = Some(Some(
                            cell.core
                                .with_mut(|core| ManuallyDrop::take(&mut (*core).output)),
                        ));
                    } else if !PANICKED && state & CLOSED != 0 {
                        Self::drop_cancelled_output(ptr);
                        unclaimed = Some(None);
                    }
                }
                // Completed, closed too with no handle, and the run's
                // reference gone in the same exchange: a handle that sees the
                // completion holds the last reference, so its drop frees the
                // task where the handle is.
                let mut new = (state & !RUNNING & !SCHEDULED) | COMPLETED;
                if state & TASK == 0 {
                    new |= CLOSED;
                } else if PANICKED {
                    new &= !CLOSED;
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
            // Nothing of the task is touched from here on, unless the run's
            // reference was the last one, with no handle.
            if last {
                Self::destroy(ptr);
            }
            // Before the awaiter learns of the end: no handle is left to wait
            // for it.
            if let Some(Some(output)) = unclaimed {
                drop_unclaimed(output);
            }
            if let Some(waker) = awaiter {
                wake_caught(waker);
            }
            false
        }
    }

    /// The output of a task whose handle cancelled, taken out of the cell
    /// and dropped by the run, out of line of a detached task's completion.
    #[cold]
    #[inline(never)]
    unsafe fn drop_cancelled_output(ptr: *const ()) {
        // SAFETY: the run's reference holds the task, the outcome is in the
        // cell, and nobody else takes it.
        unsafe {
            let output = Self::cell(ptr)
                .core
                .with_mut(|core| ManuallyDrop::take(&mut (*core).output));
            drop_unclaimed(output);
        }
    }

    /// [`complete`](Self::complete) for a panic: out of line.
    ///
    /// # Safety
    ///
    /// As [`complete`](Self::complete).
    #[cold]
    #[inline(never)]
    unsafe fn complete_panicked(ptr: *const (), state: usize) -> bool {
        // SAFETY: the caller's contract.
        unsafe { Self::complete::<true>(ptr, state) }
    }

    /// The end of a run that finds the task cancelled while it waited in the
    /// queue: the future is dropped, RUNNING cleared, and the run lets go as
    /// `finish` does.
    #[cold]
    #[inline(never)]
    unsafe fn end_queued(ptr: *const ()) -> bool {
        // SAFETY: the run's reference is live until `finish`, and the future
        // is the run's to drop.
        unsafe {
            let panic = Self::drop_future(ptr);
            let state = Self::settle(ptr, RUNNING, panic);
            Self::finish(ptr, state);
            false
        }
    }

    /// The end of a Pending run that finds the task cancelled during the
    /// poll: the canceller left the future to the run, which drops it, then
    /// clears RUNNING (and a wake's SCHEDULED) and lets go as `finish` does.
    #[cold]
    #[inline(never)]
    unsafe fn end_closed(ptr: *const ()) -> bool {
        // SAFETY: the run's reference is live until `finish`, and the future
        // is the run's to drop.
        unsafe {
            let panic = Self::drop_future(ptr);
            let state = Self::settle(ptr, RUNNING | SCHEDULED, panic);
            Self::finish(ptr, state);
            false
        }
    }
}

/// What the task takes from the end of its future: the panic of the future's
/// drop if it had one, else the terminate hook's. The hook runs either way;
/// the hook's panic after the drop's is logged.
#[cold]
#[inline(never)]
fn ended_with<S>(schedule: &S, id: Id, dropped: Option<Panic>) -> Option<Panic>
where
    S: Schedule,
{
    match (dropped, schedule.terminated(id)) {
        (None, hook) => hook,
        (dropped, None) => dropped,
        (Some(dropped), Some(hook)) => {
            lost_again(id, hook);
            Some(dropped)
        }
    }
}

/// The outcome of a run whose future's drop panicked with `panic`: the
/// poll's own panic if it had one, the drop's then logged; else the drop's,
/// the value dropped (a panic of that drop logged).
#[cold]
#[inline(never)]
fn outcome_with<T>(id: Id, output: Result<T, Panic>, panic: Panic) -> Result<T, Panic> {
    match output {
        Ok(value) => {
            if let Some(again) = caught::catch(|| drop(value)) {
                lost_again(id, again);
            }
            Err(panic)
        }
        Err(first) => {
            lost_again(id, panic);
            Err(first)
        }
    }
}

/// The poll's result after the after-poll hook ran: if it panicked, the task
/// completes with that panic, as with [`outcome_with`].
#[cold]
#[inline(never)]
fn after_poll_hooked<T, S>(
    schedule: &S,
    id: Id,
    poll: Result<Poll<T>, Thin>,
) -> Result<Poll<T>, Thin>
where
    S: Schedule,
{
    let Some(panic) = schedule.after_poll(id) else {
        return poll;
    };
    match poll {
        Ok(Poll::Pending) => Err(Box::new(panic)),
        Ok(Poll::Ready(value)) => outcome_with(id, Ok(value), panic)
            .map(Poll::Ready)
            .map_err(Box::new),
        Err(first) => outcome_with::<T>(id, Err(*first), panic)
            .map(Poll::Ready)
            .map_err(Box::new),
    }
}

/// Drops the output of a task nobody awaits: a panic as its outcome, and a
/// panic of a value's drop, are logged.
#[inline(always)]
pub(super) fn drop_unclaimed<T>(output: Result<T, Panic>) {
    match output {
        Ok(value) => {
            if let Some(panic) = caught::catch(|| drop(value)) {
                lost_output(panic);
            }
        }
        Err(panic) => lost_unawaited(panic),
    }
}

/// Logs the panic of a task nobody awaits.
#[cold]
#[inline(never)]
pub(super) fn lost_unawaited(panic: Panic) {
    caught::lost(format_args!("a task panicked and nobody awaited it"), panic);
}

/// Logs the panic of the drop of an output nobody took.
#[cold]
#[inline(never)]
fn lost_output(panic: Panic) {
    caught::lost(
        format_args!("a task's output panicked as it was dropped"),
        panic,
    );
}

/// Logs the panic of task `id`'s future's drop (or its terminate hook) that
/// nobody awaits.
#[cold]
#[inline(never)]
fn lost_ending(id: Id, panic: Panic) {
    caught::lost(format_args!("task {id} panicked as it ended"), panic);
}

/// Logs a panic of task `id` that came after the one its outcome holds.
#[cold]
#[inline(never)]
fn lost_again(id: Id, panic: Panic) {
    caught::lost(format_args!("task {id} panicked again as it ended"), panic);
}

/// Logs the panic of the drop of what a task's memory held as it was freed.
#[cold]
#[inline(never)]
fn lost_freeing(panic: Panic) {
    caught::lost(
        format_args!("a task panicked as its memory was freed"),
        panic,
    );
}
