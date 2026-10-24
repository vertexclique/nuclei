//! Where a task of a shard goes when it is woken, and the count of live
//! tasks placement reads.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use super::context;
use super::shard::Shared;
use super::task::{Runnable, Schedule, ScheduleInfo};
use crate::sync::internal::{AtomicUsize, Ordering};

/// The schedule of a task placed on a shard.
pub(crate) struct Sched {
    pub(crate) shard: Arc<Shared>,
    /// The task's future is not `Send`: it runs on the shard's thread only.
    pub(crate) bound: bool,
}

// SAFETY: the remote path holds a waker of the task across the push, so
// `self` (in the task's memory) stays alive until it is done with it.
unsafe impl Schedule for Sched {
    fn schedule(&self, runnable: Runnable, info: ScheduleInfo) {
        let remote = context::current(|current| match current {
            Some(shard) if shard.is(&self.shard) => {
                shard.push_local(runnable, info);
                None
            }
            _ => Some(runnable),
        });
        if let Some(runnable) = remote {
            // Once pushed, the shard may run the task to its end and free
            // it, this schedule with it: the waker keeps it until the push
            // and the wake are done.
            let keep = runnable.waker();
            self.shard.push_remote(runnable, self.bound);
            drop(keep);
        }
    }
}

/// A task's future, counted in its shard's live tasks until it completes
/// or is dropped.
pub(crate) struct Counted<F> {
    future: F,
    load: *const AtomicUsize,
}

// SAFETY: the pointer is to an atomic in the task's shard, which the task's
// schedule keeps alive for as long as the future lives; the future decides
// the rest.
unsafe impl<F: Send> Send for Counted<F> {}

impl<F> Counted<F> {
    /// Counts the task in `shard` from now on.
    pub(crate) fn new(future: F, shard: &Shared) -> Self {
        shard.load.fetch_add(1, Ordering::Relaxed);
        Self {
            future,
            load: &shard.load,
        }
    }
}

impl<F: Future> Future for Counted<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: the future is never moved out of its place: structural pin.
        unsafe { self.map_unchecked_mut(|c| &mut c.future) }.poll(cx)
    }
}

impl<F> Drop for Counted<F> {
    fn drop(&mut self) {
        // SAFETY: see `Send` above: the shard outlives the future.
        unsafe { (*self.load).fetch_sub(1, Ordering::Relaxed) };
    }
}
