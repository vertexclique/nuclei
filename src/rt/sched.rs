//! Where a task of a shard goes when it is woken, and the counts of live
//! tasks placement reads (a shard's) and a detached thread waits on (a
//! home's).

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use super::context;
use super::handoff::home::Home;
use super::shard::Shared;
use super::task::{Runnable, Schedule, ScheduleInfo};
use crate::sync::internal::Ordering;

/// The schedule of a task placed on a shard.
pub(crate) struct Sched {
    pub(crate) shard: Arc<Shared>,
    /// The home of a task whose future is not `Send`: it runs on its home's
    /// thread only, whichever shard that thread runs.
    pub(crate) home: Option<Arc<Home>>,
    /// Its runtime's task hooks, if it has any.
    pub(crate) hooks: Option<Arc<super::tuning::TaskHooks>>,
}

// SAFETY: the remote path holds a waker of the task across the push, so
// `self` (in the task's memory) stays alive until it is done with it.
unsafe impl Schedule for Sched {
    #[inline(always)]
    fn schedule(&self, runnable: Runnable, info: ScheduleInfo) {
        let runnable = if self.home.is_none() {
            match queue_here(&self.shard, runnable, info) {
                Some(runnable) => runnable,
                None => return,
            }
        } else {
            runnable
        };
        self.schedule_elsewhere(runnable, info);
    }

    #[inline]
    fn before_poll(&self, id: super::task::Id) {
        if let Some(hooks) = &self.hooks {
            super::tuning::call(hooks.before_poll.as_ref(), id);
        }
    }

    #[inline]
    fn after_poll(&self, id: super::task::Id) {
        if let Some(hooks) = &self.hooks {
            super::tuning::call(hooks.after_poll.as_ref(), id);
        }
    }

    #[inline]
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

/// Queues `runnable`, of a task of `shard` that is not bound to a thread, on
/// that shard if this thread runs it and needs no bracket to reach it (the
/// common case, a task woken or spawned on its own shard's thread): what
/// [`Sched::schedule`]'s general path does then. Hands it back otherwise.
#[inline(always)]
pub(crate) fn queue_here(
    shard: &Arc<Shared>,
    runnable: Runnable,
    info: ScheduleInfo,
) -> Option<Runnable> {
    let here = context::local();
    match context::running_inside(here) {
        Some(current) if current.is(shard) => {
            current.push_local_from(runnable, info, here.in_task());
            None
        }
        _ => Some(runnable),
    }
}

impl Sched {
    /// [`Schedule::schedule`] for a bound task, or one woken anywhere but
    /// inside its own shard.
    #[inline(never)]
    fn schedule_elsewhere(&self, runnable: Runnable, info: ScheduleInfo) {
        if let Some(home) = &self.home {
            let remote = context::current(|current| match current {
                Some(shard) if context::is_home(home) || shard.owns_home(home) => {
                    shard.push_local(runnable, info);
                    None
                }
                _ => Some(runnable),
            });
            if let Some(runnable) = remote {
                // As below: the waker keeps the task until the push is done.
                let keep = runnable.waker();
                home.push(runnable);
                drop(keep);
            }
            return;
        }
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
            self.shard.push_remote(runnable);
            drop(keep);
        }
    }
}

/// A task's future, counted in its shard's live tasks until it completes
/// or is dropped, and dropped inside the shard's runtime wherever that is.
pub(crate) struct Counted<F> {
    future: core::mem::ManuallyDrop<F>,
    shard: *const Shared,
}

// SAFETY: the pointer is to the task's shard, which the task's schedule
// keeps alive for as long as the future lives; the future decides the rest.
unsafe impl<F: Send> Send for Counted<F> {}

impl<F> Counted<F> {
    /// Counts the task in `shard` from now on.
    pub(crate) fn new(future: F, shard: &Shared) -> Self {
        shard.load.fetch_add(1, Ordering::Relaxed);
        Self {
            future: core::mem::ManuallyDrop::new(future),
            shard,
        }
    }
}

impl<F: Future> Future for Counted<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: the future is never moved out of its place: structural pin.
        unsafe { self.map_unchecked_mut(|c| &mut *c.future) }.poll(cx)
    }
}

impl<F> Drop for Counted<F> {
    fn drop(&mut self) {
        // SAFETY: see `Send` above: the shard outlives the future.
        let shard = unsafe { &*self.shard };
        {
            // Woken after its shard stopped, a task is dropped on its
            // waker's thread: what its drop hands on (a dropped handle's
            // bytes) still goes to its runtime (proofs/tla/going/Linger.tla).
            let _runtime = shard.enter_runtime();
            // SAFETY: dropped once, here, in its place.
            unsafe { core::mem::ManuallyDrop::drop(&mut self.future) };
        }
        if shard.load.fetch_sub(1, Ordering::Release) == 1 {
            shard.last_task_done();
        }
    }
}

/// A bound task's future, counted in its home's live tasks until it
/// completes or is dropped.
pub(crate) struct AtHome<F> {
    future: F,
    home: *const Home,
}

impl<F> AtHome<F> {
    /// Counts the task in `home` from now on.
    pub(crate) fn new(future: F, home: &Home) -> Self {
        home.began();
        Self { future, home }
    }
}

impl<F: Future> Future for AtHome<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: the future is never moved out of its place: structural pin.
        unsafe { self.map_unchecked_mut(|c| &mut c.future) }.poll(cx)
    }
}

impl<F> Drop for AtHome<F> {
    fn drop(&mut self) {
        // SAFETY: the task's schedule holds the home for as long as the
        // future lives.
        unsafe { &*self.home }.ended();
    }
}
