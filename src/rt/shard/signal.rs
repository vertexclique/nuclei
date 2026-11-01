//! The waker of a shard's `block_on` future.

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::Deref;
use core::task::{RawWaker, RawWakerVTable, Waker};
use std::sync::Arc;

use super::Shared;
use crate::rt::handoff::target::Target;
use crate::sync::internal::{AtomicBool, Ordering};

/// The waker of a `block_on` future: marks it woken and wakes the shard it
/// runs on, which moves when its shard is taken from its thread (see
/// `shard::run`).
pub(crate) struct Signal {
    pub(super) woken: AtomicBool,
    pub(super) target: Target,
}

impl Signal {
    /// A future's waker on the shard of `shared`, woken once so its first
    /// poll comes at once.
    pub(super) fn new(shared: &Arc<Shared>) -> Self {
        Self {
            woken: AtomicBool::new(true),
            target: Target::new(Arc::clone(shared)),
        }
    }

    /// Makes the signal, which nothing else holds, a new future's: woken once,
    /// as [`new`](Self::new) makes it. False for one whose target moved,
    /// which stays its first shard's no more.
    pub(super) fn reset(&mut self) -> bool {
        if self.target.moved() {
            return false;
        }
        self.woken.store(true, Ordering::Relaxed);
        true
    }

    /// The waker the loop polls with, lending `signal`'s count: it is never
    /// dropped, and each clone takes a count of its own.
    pub(super) fn waker(signal: &Arc<Self>) -> Lent<'_> {
        let data = Arc::as_ptr(signal).cast::<()>();
        // SAFETY: `data` is a live `Arc<Signal>`'s, as `VTABLE` takes it;
        // the waker, never dropped, spends no count, and `Lent` keeps it
        // within the borrow of `signal`.
        let waker = unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) };
        Lent(ManuallyDrop::new(waker), PhantomData)
    }

    fn wake_by_ref(&self) {
        // Sequentially consistent, as the target's read: either the thread
        // moving the future to its own shard finds the flag, or this wakes
        // that shard.
        self.woken.store(true, Ordering::SeqCst);
        self.target.get().unpark();
    }
}

/// A waker of a signal that borrows its count (see [`Signal::waker`]).
pub(crate) struct Lent<'a>(ManuallyDrop<Waker>, PhantomData<&'a Arc<Signal>>);

impl Deref for Lent<'_> {
    type Target = Waker;

    fn deref(&self) -> &Waker {
        &self.0
    }
}

/// The table of a signal's wakers: each but the lent one holds a count of
/// its `Arc<Signal>`.
static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

unsafe fn clone(data: *const ()) -> RawWaker {
    // SAFETY: a waker of the table refers to a live signal; the clone's count
    // is taken here.
    unsafe { Arc::increment_strong_count(data.cast::<Signal>()) };
    RawWaker::new(data, &VTABLE)
}

unsafe fn wake(data: *const ()) {
    // SAFETY: the waker's count, spent here.
    let signal = unsafe { Arc::from_raw(data.cast::<Signal>()) };
    signal.wake_by_ref();
}

unsafe fn wake_by_ref(data: *const ()) {
    // SAFETY: a waker of the table refers to a live signal.
    unsafe { (*data.cast::<Signal>()).wake_by_ref() };
}

unsafe fn drop_waker(data: *const ()) {
    // SAFETY: the waker's count, spent here.
    unsafe { Arc::decrement_strong_count(data.cast::<Signal>()) };
}
