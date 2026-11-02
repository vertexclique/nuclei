//! Where a task of a shard goes when it is woken, and the counts of live
//! tasks placement reads (a shard's) and a detached thread waits on (a
//! home's).

use core::mem::ManuallyDrop;
use std::sync::Arc;

use super::caught::{self, Panic};
use super::context::{self, Bracketed};
use super::handoff::home::Home;
use super::shard::Shared;
use super::task::{Runnable, Schedule, ScheduleInfo};
use crate::sync::internal::Ordering;

/// The schedule of a task placed on a shard. Its fields are let go of in
/// its own drop, inline in the task's destroy, rather than in a drop glue
/// called there.
pub(crate) struct Sched {
    shard: ManuallyDrop<Arc<Shared>>,
    /// The home of a task whose future is not `Send`: it runs on its home's
    /// thread only, whichever shard that thread runs.
    home: ManuallyDrop<Option<Arc<Home>>>,
    /// Its runtime's task hooks, if it has any.
    hooks: ManuallyDrop<Option<Arc<super::tuning::TaskHooks>>>,
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
            shard: ManuallyDrop::new(shard),
            home: ManuallyDrop::new(home),
            hooks: ManuallyDrop::new(hooks),
            counted: true,
        }
    }

    /// The schedule of a task of the runtime's own on `shard`: left out of
    /// the shard's count, it keeps no worker from draining, and has no hooks.
    #[cfg(unix)]
    pub(crate) fn uncounted(shard: Arc<Shared>) -> Self {
        Self {
            shard: ManuallyDrop::new(shard),
            home: ManuallyDrop::new(None),
            hooks: ManuallyDrop::new(None),
            counted: false,
        }
    }
}

impl Drop for Sched {
    #[inline(always)]
    fn drop(&mut self) {
        // SAFETY: each field is dropped once, here, and never used again.
        unsafe {
            ManuallyDrop::drop(&mut self.shard);
            ManuallyDrop::drop(&mut self.home);
            ManuallyDrop::drop(&mut self.hooks);
        }
    }
}

// SAFETY: the remote path holds a waker of the task across the push, so
// `self` (in the task's memory) stays alive until it is done with it.
unsafe impl Schedule for Sched {
    #[inline(always)]
    fn schedule(&self, runnable: Runnable, info: ScheduleInfo) {
        let left = match &*self.home {
            None => queue_here(&self.shard, runnable, info),
            Some(home) => queue_home_here(home, runnable, info),
        };
        if let Some(runnable) = left {
            self.schedule_elsewhere(runnable);
        }
    }

    #[inline(always)]
    fn drop_future(&self, drop: impl FnOnce()) {
        if !self.counted {
            return drop();
        }
        {
            // Woken after its shard stopped, a task is dropped on its
            // waker's thread: what its drop hands on (a dropped handle's
            // bytes) still goes to its runtime (proofs/tla/going/Linger.tla).
            let _runtime = self.shard.enter_runtime();
            if let Some(home) = &*self.home {
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
        if let Some(hooks) = &*self.hooks {
            super::tuning::call(hooks.before_poll.as_ref(), id);
        }
    }

    #[inline]
    fn after_poll(&self, id: super::task::Id) -> Option<Panic> {
        match &*self.hooks {
            Some(hooks) => super::tuning::call_caught(hooks.after_poll.as_ref(), id),
            None => None,
        }
    }

    #[inline]
    fn terminated(&self, id: super::task::Id) -> Option<Panic> {
        match &*self.hooks {
            Some(hooks) => super::tuning::call_caught(hooks.terminate.as_ref(), id),
            None => None,
        }
    }

    fn panicked(&self, _id: super::task::Id) {
        // In the middle of the task's change of state, nothing here may
        // unwind: not the shutdown, nor the drop of the last handle of the
        // runtime that may come with it.
        let shut = caught::catch(|| {
            let runtime = self
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.shutdown.as_ref());
            if let Some(runtime) = runtime.and_then(std::sync::Weak::upgrade) {
                runtime.shut_down_on_panic();
            }
        });
        if let Some(panic) = shut {
            caught::lost(
                format_args!("the runtime's shutdown after a task's panic panicked"),
                panic,
            );
        }
    }
}

/// Queues `runnable`, of a task of `shard` that is not bound to a thread, on
/// that shard if this thread runs it (the common case, a task woken or
/// spawned on its own shard's thread): what [`Sched::schedule`] does then.
/// Hands it back otherwise.
#[inline(always)]
pub(crate) fn queue_here(
    shard: &Arc<Shared>,
    runnable: Runnable,
    info: ScheduleInfo,
) -> Option<Runnable> {
    // The address alone, which both ways compare: no reference to the schedule's field is
    // kept for the way out of line.
    let shard = Arc::as_ptr(shard) as usize;
    match context::unbracketed(context::local()) {
        Ok(current) => queue_on(current, shard, runnable, info),
        Err(Bracketed) => queue_here_bracketed(shard, runnable, info),
    }
}

/// [`queue_here`] from a task's code on a shard the monitor watches: the leaf push into an empty
/// LIFO slot (`context::push_lifo_marked`), else the full bracket. Its one call is the full
/// bracket's, in tail position, so the push saves only the registers that call's arguments keep.
#[cold]
#[inline(never)]
fn queue_here_bracketed(shard: usize, runnable: Runnable, info: ScheduleInfo) -> Option<Runnable> {
    match context::push_lifo_marked(runnable, info, |current| current.shared_addr() == shard) {
        Ok(()) => None,
        Err(runnable) => queue_here_held(shard, runnable, info),
    }
}

/// [`queue_here_bracketed`] for any push but the leaf's, in a full bracket.
#[cold]
#[inline(never)]
fn queue_here_held(shard: usize, runnable: Runnable, info: ScheduleInfo) -> Option<Runnable> {
    let held = context::bracketed();
    queue_on(held.shard(), shard, runnable, info)
}

/// [`queue_here`] on `current`, the shard this thread runs as a call into it finds it, for the
/// shard whose `Shared` is at `shard`.
#[inline(always)]
fn queue_on(
    current: Option<&super::shard::Shard>,
    shard: usize,
    runnable: Runnable,
    info: ScheduleInfo,
) -> Option<Runnable> {
    match current {
        Some(current) if current.shared_addr() == shard => {
            current.push_local_from(runnable, info, context::in_task());
            None
        }
        _ => Some(runnable),
    }
}

/// Queues `runnable`, of a task bound to `home`, on the shard this thread
/// runs if the thread is the home's (or runs the shard whose own home it
/// is): what [`Sched::schedule`] does then, for a task woken or spawned on
/// its home's thread. Hands it back otherwise.
#[inline(always)]
pub(crate) fn queue_home_here(
    home: &Home,
    runnable: Runnable,
    info: ScheduleInfo,
) -> Option<Runnable> {
    match context::unbracketed(context::local()) {
        Ok(current) => queue_home_on(current, home, runnable, info),
        Err(Bracketed) => queue_home_here_bracketed(home, runnable, info),
    }
}

/// [`queue_home_here`] from a task's code on a shard the monitor watches.
#[cold]
#[inline(never)]
fn queue_home_here_bracketed(
    home: &Home,
    runnable: Runnable,
    info: ScheduleInfo,
) -> Option<Runnable> {
    let held = context::bracketed();
    queue_home_on(held.shard(), home, runnable, info)
}

/// [`queue_home_here`] on `current`, the shard this thread runs as a call into it finds it.
#[inline(always)]
fn queue_home_on(
    current: Option<&super::shard::Shard>,
    home: &Home,
    runnable: Runnable,
    info: ScheduleInfo,
) -> Option<Runnable> {
    match current {
        Some(current) if context::is_home(home) || current.owns_home(home) => {
            current.push_local_from(runnable, info, context::in_task());
            None
        }
        _ => Some(runnable),
    }
}

impl Sched {
    /// [`Schedule::schedule`] for a task woken anywhere but on its own
    /// shard's thread, or its home's for a bound one.
    #[inline(never)]
    fn schedule_elsewhere(&self, runnable: Runnable) {
        // Once pushed, the shard may run the task to its end and free it,
        // this schedule with it: the waker keeps it until the push and the
        // wake are done.
        let keep = runnable.waker();
        match &*self.home {
            Some(home) => home.push(runnable),
            None => self.shard.push_remote(runnable),
        }
        drop(keep);
    }
}
