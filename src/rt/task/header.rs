//! The first field of every task: the state word, the awaiter's waker and
//! the table of the task's functions.
//!
//! The awaiter is the waker of whoever awaits the [`Task`](super::Task)
//! handle. Storing it and taking it out race each other (the handle polls on
//! one thread while the task completes on another), so both go through the
//! state word: [`REGISTERING`] while the handle stores, [`NOTIFYING`] while a
//! completion or a close takes it, and whichever side finds the other's flag
//! leaves the waker to it. [`AWAITER`] says a waker is stored, so a
//! completion with nobody waiting reads one bit and moves on.

use core::task::{RawWakerVTable, Waker};

use super::abort_on_panic;
use super::raw::TaskVTable;
use super::state::{AWAITER, NOTIFYING, REFERENCE, REGISTERING, SCHEDULED, TASK};
use crate::sync::internal::{AtomicPtr, AtomicUsize, Ordering, UnsafeCell};

/// The type-erased head of a task's memory; the task pointer points at it.
pub(super) struct Header {
    pub(super) state: AtomicUsize,
    /// The waker of the handle's awaiter; see the module docs.
    awaiter: UnsafeCell<Option<Waker>>,
    /// The data and table of the waker last stored as the awaiter, written
    /// under REGISTERING: while AWAITER is set they name the waker the cell
    /// holds, or that the notifier holding NOTIFYING took and wakes.
    awaiter_data: AtomicPtr<()>,
    awaiter_table: AtomicPtr<RawWakerVTable>,
    pub(super) vtable: &'static TaskVTable,
    /// The task's id, unique in the process.
    pub(super) id: super::Id,
    /// Its place in a shard's inbox, which it is in at most once (see
    /// `rt::inbox`).
    pub(super) link: crate::rt::inbox::Link,
    /// The thread its memory came from (see `mem`).
    pub(super) owner: crate::mem::Owner,
}

/// The next task id: ids start at 1 and are never reused (a `u64` does not
/// wrap in the life of a process).
static NEXT_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

impl Header {
    /// A new task's header: scheduled, with its handle and one reference,
    /// the `Runnable` that spawning returns, and the next id.
    pub(super) fn new(vtable: &'static TaskVTable, owner: crate::mem::Owner) -> Self {
        let id = NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Self {
            state: AtomicUsize::new(SCHEDULED | TASK | REFERENCE),
            awaiter: UnsafeCell::new(None),
            awaiter_data: AtomicPtr::new(core::ptr::null_mut()),
            awaiter_table: AtomicPtr::new(core::ptr::null_mut()),
            vtable,
            id: super::Id(core::num::NonZeroU64::new(id).unwrap_or(core::num::NonZeroU64::MIN)),
            link: crate::rt::inbox::Link::new(),
            owner,
        }
    }

    /// Takes the awaiter out and wakes it, unless it would wake `current`.
    pub(super) fn notify(&self, current: Option<&Waker>) {
        if let Some(waker) = self.take(current) {
            abort_on_panic(|| waker.wake());
        }
    }

    /// Takes the awaiter out, leaving none. Returns nothing when the waker is
    /// `current`'s (it is dropped here) or when a registration is in
    /// progress, which then wakes it itself.
    #[inline]
    pub(super) fn take(&self, current: Option<&Waker>) -> Option<Waker> {
        let state = self.state.fetch_or(NOTIFYING, Ordering::AcqRel);
        if state & (NOTIFYING | REGISTERING) != 0 {
            return None;
        }
        // SAFETY: NOTIFYING is ours and REGISTERING is clear, so nobody else
        // reaches the cell until the flag drops below.
        let waker = self.awaiter.with_mut(|cell| unsafe { (*cell).take() });
        self.state
            .fetch_and(!NOTIFYING & !AWAITER, Ordering::Release);
        match (waker, current) {
            (Some(waker), Some(current)) if waker.will_wake(current) => {
                abort_on_panic(|| drop(waker));
                None
            }
            (waker, _) => waker,
        }
    }

    /// Claims the awaiter's cell for a completion: the waker it holds,
    /// whether the cell was the caller's, whose next exchange of the word
    /// then clears NOTIFYING and AWAITER (the flag held until then, so a
    /// registration meanwhile wakes its own waker), and the word as the
    /// claim left it. With a registration or a notification in progress the
    /// caller takes nothing and leaves the flags to it, as
    /// [`take`](Self::take) does.
    #[inline(always)]
    pub(super) fn claim(&self) -> (Option<Waker>, bool, usize) {
        let state = self.state.fetch_or(NOTIFYING, Ordering::AcqRel);
        if state & (NOTIFYING | REGISTERING) != 0 {
            return (None, false, state | NOTIFYING);
        }
        // SAFETY: NOTIFYING is ours and REGISTERING is clear, so nobody else
        // reaches the cell until the caller's exchange clears the flag.
        let waker = self.awaiter.with_mut(|cell| unsafe { (*cell).take() });
        (waker, true, state | NOTIFYING)
    }

    /// Whether the waker last stored wakes as `waker` does (`will_wake`):
    /// read while AWAITER is set, which publishes it.
    #[inline]
    fn stored_wakes_as(&self, waker: &Waker) -> bool {
        core::ptr::eq(self.awaiter_data.load(Ordering::Relaxed), waker.data())
            && core::ptr::eq(self.awaiter_table.load(Ordering::Relaxed), waker.vtable())
    }

    /// Stores `waker` as the awaiter. A notification that raced the store is
    /// handled here: the waker is woken at once instead of being left behind.
    #[inline(always)]
    pub(super) fn register(&self, waker: &Waker) {
        // A stale first look costs the exchange below a retry, no more.
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            debug_assert!(state & REGISTERING == 0, "one awaiter registers at a time");
            if state & NOTIFYING != 0 {
                // A notification holds the cell. With a waker stored that
                // wakes this one's task, its wake is this registration's;
                // otherwise this waker would miss it, so it is woken now and
                // polls again.
                if state & AWAITER == 0 || !self.stored_wakes_as(waker) {
                    abort_on_panic(|| waker.wake_by_ref());
                }
                return;
            }
            match self.state.compare_exchange_weak(
                state,
                state | REGISTERING,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    state |= REGISTERING;
                    break;
                }
                Err(actual) => state = actual,
            }
        }

        // SAFETY: REGISTERING is ours, and a notifier that comes now finds it
        // and leaves the cell alone.
        let old = self
            .awaiter
            .with_mut(|cell| unsafe { (*cell).replace(waker.clone()) });
        // Published by the exchange below that sets AWAITER.
        self.awaiter_data
            .store(waker.data().cast_mut(), Ordering::Relaxed);
        self.awaiter_table.store(
            core::ptr::from_ref(waker.vtable()).cast_mut(),
            Ordering::Relaxed,
        );
        abort_on_panic(|| drop(old));

        let mut missed = None;
        loop {
            // A notifier came while the waker was stored: it took nothing, so
            // the waker is taken and woken here.
            if state & NOTIFYING != 0 && missed.is_none() {
                // SAFETY: REGISTERING is still ours.
                missed = self.awaiter.with_mut(|cell| unsafe { (*cell).take() });
            }
            let new = if missed.is_none() {
                (state & !NOTIFYING & !REGISTERING) | AWAITER
            } else {
                state & !NOTIFYING & !REGISTERING & !AWAITER
            };
            match self
                .state
                .compare_exchange_weak(state, new, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(actual) => state = actual,
            }
        }
        if let Some(waker) = missed {
            abort_on_panic(|| waker.wake());
        }
    }
}
