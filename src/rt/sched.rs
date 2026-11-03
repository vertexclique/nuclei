//! Where a task of a shard goes when it is woken, and the counts of live
//! tasks placement reads (a shard's) and a detached thread waits on (a
//! home's).

use core::mem::ManuallyDrop;
use std::sync::Arc;

use super::caught::{self, Panic};
use super::context::{self, Bracketed};
use super::handoff::home::Home;
use super::shard::Shared;
use super::task::list::Links;
use super::task::{Runnable, Schedule, ScheduleInfo};
use crate::sync::internal::Ordering;

/// The schedule of a task placed on a shard. `BOUND`: the task is bound to its home's thread,
/// known at compile time so the paths of one kind carry nothing of the other's. Its fields are let go of in
/// its own drop, inline in the task's destroy, rather than in a drop glue
/// called there.
pub(crate) struct Sched<const BOUND: bool> {
    shard: ManuallyDrop<Arc<Shared>>,
    /// The home of a task whose future is not `Send`: it runs on its home's
    /// thread only, whichever shard that thread runs.
    home: ManuallyDrop<Option<Arc<Home>>>,
    /// Its runtime's task hooks, if it has any.
    hooks: ManuallyDrop<Option<Arc<super::tuning::TaskHooks>>>,
    /// Whether the task counts in its shard's live tasks (and, bound, in its home's) until its
    /// future is dropped, which happens inside the shard's runtime wherever that is
    /// (proofs/tla/going/Linger.tla), or, a visitor's, inside the runtime it visits for.
    kind: Kind,
}

/// What a schedule's task is, for the drop of its future: one test on the common kind.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Counted,
    Uncounted,
    /// Counted, and its hooks name the runtime it visits for ([`Visiting`]).
    Visitor,
}

impl<const BOUND: bool> Sched<BOUND> {
    /// The schedule of a task of `shard`, bound to `home` if it is not
    /// `Send`, counted in the shard's live tasks (and the home's) from now
    /// on.
    pub(crate) fn counted(
        shard: Arc<Shared>,
        home: Option<Arc<Home>>,
        hooks: Option<Arc<super::tuning::TaskHooks>>,
    ) -> Self {
        debug_assert_eq!(home.is_some(), BOUND);
        shard.load.fetch_add(1, Ordering::Relaxed);
        Self {
            shard: ManuallyDrop::new(shard),
            home: ManuallyDrop::new(home),
            hooks: ManuallyDrop::new(hooks),
            kind: Kind::Counted,
        }
    }

    /// The runtime a visitor's future is dropped inside: the one it visits for.
    #[cold]
    #[inline(never)]
    fn visited_runtime(&self) -> Option<context::RuntimeSet> {
        let visiting = self.hooks.as_ref().as_ref()?.visiting.as_deref()?;
        Some(context::set_runtime(&visiting.inner))
    }
}

impl Sched<false> {
    /// The schedule of a task of the runtime's own on `shard`: left out of
    /// the shard's count, it keeps no worker from draining, and has no hooks.
    pub(crate) fn uncounted(shard: Arc<Shared>) -> Self {
        Self {
            shard: ManuallyDrop::new(shard),
            home: ManuallyDrop::new(None),
            hooks: ManuallyDrop::new(None),
            kind: Kind::Uncounted,
        }
    }

    /// The schedule of a visitor on `shard`, counted as [`counted`](Sched::counted), whose
    /// `hooks` name the runtime it visits for.
    pub(crate) fn visitor(shard: Arc<Shared>, hooks: Arc<super::tuning::TaskHooks>) -> Self {
        debug_assert!(hooks.visiting.is_some());
        let mut sched = Self::counted(shard, None, Some(hooks));
        sched.kind = Kind::Visitor;
        sched
    }
}

impl<const BOUND: bool> Drop for Sched<BOUND> {
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
unsafe impl<const BOUND: bool> Schedule for Sched<BOUND> {
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
    fn pending(&self, links: &Links) {
        if !BOUND && !links.is_listed() {
            self.list(links);
        }
    }

    #[inline(always)]
    fn drop_future(&self, links: &Links, drop: impl FnOnce()) {
        if BOUND {
            // SAFETY: a bound task is listed from its spawn until its future is dropped, which
            // happens on its home's thread only.
            unsafe { links.unlink() };
            if let Some(home) = &*self.home {
                // SAFETY: as above.
                unsafe { home.task_ended() };
            }
        } else if links.is_listed() {
            self.unlist(links);
        }
        {
            // Woken after its shard stopped, a task is dropped on its
            // waker's thread: what its drop hands on (a dropped handle's
            // bytes) still goes to its runtime (proofs/tla/going/Linger.tla).
            // A visitor's goes to the runtime it visits for.
            let _runtime = match self.kind {
                Kind::Counted => self.shard.enter_runtime(),
                Kind::Visitor => self.visited_runtime(),
                Kind::Uncounted => return drop(),
            };
            drop();
        }
        if self.shard.load.fetch_sub(1, Ordering::Release) == 1 {
            self.shard.last_task_done();
        }
    }

    #[inline]
    fn before_poll(&self, id: super::task::Id) {
        if let Some(hooks) = &*self.hooks {
            if let Some(visiting) = &hooks.visiting {
                visiting.enter();
            }
            super::tuning::call(hooks.before_poll.as_ref(), id);
        }
    }

    #[inline(always)]
    fn has_hooks(&self) -> bool {
        self.hooks.is_some()
    }

    #[inline]
    fn after_poll(&self, id: super::task::Id) -> Option<Panic> {
        match &*self.hooks {
            Some(hooks) => {
                let panic = super::tuning::call_task_caught(hooks.after_poll.as_ref(), id);
                if let Some(visiting) = &hooks.visiting {
                    visiting.leave();
                }
                panic
            }
            None => None,
        }
    }

    #[inline]
    fn terminated(&self, id: super::task::Id) -> Option<Panic> {
        match &*self.hooks {
            Some(hooks) => super::tuning::call_task_caught(hooks.terminate.as_ref(), id),
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

impl<const BOUND: bool> Sched<BOUND> {
    /// Puts a task of the shard on its list, at its first poll that returned pending.
    #[cold]
    #[inline(never)]
    fn list(&self, links: &Links) {
        // SAFETY: a task of the shard is polled only by the shard's runner, its list's owner.
        unsafe { self.shard.tasks.push(links) }
    }

    /// Takes a task of the shard off its list as its future is dropped.
    #[cold]
    #[inline(never)]
    fn unlist(&self, links: &Links) {
        // SAFETY: a listed task's future is dropped only on its list owner's thread.
        unsafe { links.unlink() };
    }

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

/// The runtime of a visitor: a task that a runtime with no worker spawned onto a shard whose
/// loop serves another runtime. It is current for each poll of the task, what the poll found
/// put back after it, and for the drop of the task's future (`proofs/tla/nested/Nested.tla`,
/// `TaskSeesOwn`). Its sleeps wait in the wheels in place, the loop's (`W_visitor_wheels`).
pub(crate) struct Visiting {
    inner: Arc<super::runtime::Inner>,
    /// What the poll running now found current (`Arc::into_raw`, 0 for none), or `OUT` between
    /// polls: one poll of a task runs at a time.
    found: core::sync::atomic::AtomicUsize,
    /// The poll running now marked the thread's own shard with what it found (`mark_own`).
    marked: core::sync::atomic::AtomicBool,
}

/// [`Visiting::found`] between polls.
const OUT: usize = usize::MAX;

impl Visiting {
    pub(crate) fn new(inner: Arc<super::runtime::Inner>) -> Self {
        Self {
            inner,
            found: core::sync::atomic::AtomicUsize::new(OUT),
            marked: core::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Makes the visited runtime current for a poll; on a thread's own shard, what it found,
    /// the loop's, is marked as the one the shard serves.
    fn enter(&self) {
        if let Some(found) = context::swap_runtime(&self.inner) {
            let marked = found.as_ref().is_some_and(context::mark_own);
            self.marked
                .store(marked, core::sync::atomic::Ordering::Relaxed);
            let raw = found.map_or(0, |found| Arc::into_raw(found) as usize);
            self.found.store(raw, core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Puts back what the poll found current, and takes back its mark.
    fn leave(&self) {
        if self
            .marked
            .swap(false, core::sync::atomic::Ordering::Relaxed)
        {
            context::unmark_own();
        }
        let raw = self.found.swap(OUT, core::sync::atomic::Ordering::Relaxed);
        if raw != OUT {
            context::put_runtime(Self::taken(raw));
        }
    }

    /// The runtime `enter` kept as `raw`.
    fn taken(raw: usize) -> Option<Arc<super::runtime::Inner>> {
        // SAFETY: a nonzero `raw` came from `Arc::into_raw` in `enter`, taken back once.
        (raw != 0).then(|| unsafe { Arc::from_raw(raw as *const super::runtime::Inner) })
    }
}

impl Drop for Visiting {
    fn drop(&mut self) {
        let raw = *self.found.get_mut();
        if raw != OUT {
            drop(Self::taken(raw));
        }
    }
}
