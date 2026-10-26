//! Where a task of a shard goes when it is woken, and the count of live
//! tasks placement reads.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use super::context;
use super::shard::Shared;
use super::task::{Runnable, Schedule, ScheduleInfo};
use crate::sync::internal::Ordering;

/// The schedule of a task placed on a shard.
pub(crate) struct Sched {
    pub(crate) shard: Arc<Shared>,
    /// The task's future is not `Send`: it runs on the shard's thread only.
    pub(crate) bound: bool,
    /// Its runtime's task hooks, if it has any.
    pub(crate) hooks: Option<Arc<super::tuning::TaskHooks>>,
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

    fn before_poll(&self, id: super::task::Id) {
        if let Some(hooks) = &self.hooks {
            super::tuning::call(hooks.before_poll.as_ref(), id);
        }
    }

    fn after_poll(&self, id: super::task::Id) {
        if let Some(hooks) = &self.hooks {
            super::tuning::call(hooks.after_poll.as_ref(), id);
        }
    }

    fn terminated(&self, id: super::task::Id) {
        if let Some(hooks) = &self.hooks {
            super::tuning::call(hooks.terminate.as_ref(), id);
        }
    }

    fn panicked(&self, _id: super::task::Id) {
        let runtime = self
            .hooks
            .as_ref()
            .and_then(|hooks| hooks.shutdown.as_ref());
        if let Some(runtime) = runtime.and_then(std::sync::Weak::upgrade) {
            // In the middle of the task's change of state: nothing here may
            // unwind.
            super::task::abort_on_panic(|| runtime.shut_down_on_panic());
        }
    }
}

/// A task's future, counted in its shard's live tasks until it completes
/// or is dropped.
pub(crate) struct Counted<F> {
    future: F,
    shard: *const Shared,
}

// SAFETY: the pointer is to the task's shard, which the task's schedule
// keeps alive for as long as the future lives; the future decides the rest.
unsafe impl<F: Send> Send for Counted<F> {}

impl<F> Counted<F> {
    /// Counts the task in `shard` from now on.
    pub(crate) fn new(future: F, shard: &Shared) -> Self {
        shard.load.fetch_add(1, Ordering::Relaxed);
        Self { future, shard }
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
        let shard = unsafe { &*self.shard };
        if shard.load.fetch_sub(1, Ordering::Release) == 1 {
            shard.last_task_done();
        }
    }
}
