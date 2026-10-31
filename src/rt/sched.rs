//! Where a task of a shard goes when it is woken, and the counts of live
//! tasks placement reads (a shard's) and a detached thread waits on (a
//! home's).

use std::sync::Arc;

use super::context;
use super::handoff::home::Home;
use super::shard::Shared;
use super::task::{Runnable, Schedule, ScheduleInfo};
use crate::sync::internal::Ordering;

/// The schedule of a task placed on a shard.
pub(crate) struct Sched {
    shard: Arc<Shared>,
    /// The home of a task whose future is not `Send`: it runs on its home's
    /// thread only, whichever shard that thread runs.
    home: Option<Arc<Home>>,
    /// Its runtime's task hooks, if it has any.
    hooks: Option<Arc<super::tuning::TaskHooks>>,
    /// The task counts in its shard's live tasks (and, bound, in its home's)
    /// until its future is dropped, which happens inside the shard's
    /// runtime wherever that is (proofs/tla/going/Linger.tla).
    counted: bool,
}

impl Sched {
    /// The schedule of a task of `shard`, bound to `home` if it is not
    /// `Send`, counted in the shard's live tasks (and the home's) from now
    /// on.
    pub(crate) fn counted(
        shard: Arc<Shared>,
        home: Option<Arc<Home>>,
        hooks: Option<Arc<super::tuning::TaskHooks>>,
    ) -> Self {
        if let Some(home) = &home {
            home.began();
        }
        shard.load.fetch_add(1, Ordering::Relaxed);
        Self {
            shard,
            home,
            hooks,
            counted: true,
        }
    }

    /// The schedule of a task of the runtime's own on `shard`: left out of
    /// the shard's count, it keeps no worker from draining, and has no hooks.
    #[cfg(unix)]
    pub(crate) fn uncounted(shard: Arc<Shared>) -> Self {
        Self {
            shard,
            home: None,
            hooks: None,
            counted: false,
        }
    }
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
    fn drop_future(&self, drop: impl FnOnce()) {
        if !self.counted {
            return drop();
        }
        {
            // Woken after its shard stopped, a task is dropped on its
            // waker's thread: what its drop hands on (a dropped handle's
            // bytes) still goes to its runtime (proofs/tla/going/Linger.tla).
            let _runtime = self.shard.enter_runtime();
            if let Some(home) = &self.home {
                home.ended();
            }
            drop();
        }
        if self.shard.load.fetch_sub(1, Ordering::Release) == 1 {
            self.shard.last_task_done();
        }
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
