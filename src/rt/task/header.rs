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

use core::task::Waker;

use super::abort_on_panic;
use super::raw::TaskVTable;
use super::state::{AWAITER, NOTIFYING, REFERENCE, REGISTERING, SCHEDULED, TASK};
use crate::sync::internal::{AtomicUsize, Ordering, UnsafeCell};

/// The type-erased head of a task's memory; the task pointer points at it.
pub(super) struct Header {
    pub(super) state: AtomicUsize,
    /// The waker of the handle's awaiter; see the module docs.
    awaiter: UnsafeCell<Option<Waker>>,
    pub(super) vtable: &'static TaskVTable,
}

impl Header {
    /// A new task's header: scheduled, with its handle and one reference,
    /// the `Runnable` that spawning returns.
    pub(super) fn new(vtable: &'static TaskVTable) -> Self {
        Self {
            state: AtomicUsize::new(SCHEDULED | TASK | REFERENCE),
            awaiter: UnsafeCell::new(None),
            vtable,
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

    /// Stores `waker` as the awaiter. A notification that raced the store is
    /// handled here: the waker is woken at once instead of being left behind.
    pub(super) fn register(&self, waker: &Waker) {
        let mut state = self.state.fetch_or(0, Ordering::Acquire);
        loop {
            debug_assert!(state & REGISTERING == 0, "one awaiter registers at a time");
            if state & NOTIFYING != 0 {
                // A notification is taking the old waker: this one would
                // miss it, so it is woken now and polls again.
                abort_on_panic(|| waker.wake_by_ref());
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
