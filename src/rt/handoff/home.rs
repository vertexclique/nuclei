//! The home of a runner's bound tasks, the ones `spawn_local` keeps on the
//! thread that spawned them.
//!
//! A bound task stays with its thread, whichever shard the thread runs: its
//! runnables pushed from other threads wait in the home's inbox, which only
//! the home's thread takes from, on whatever shard it runs. The home names
//! the shard to wake (its [`Target`]): the worker shard its thread runs,
//! until that shard is taken from the thread, then the thread's own. A
//! thread makes a new home each time it starts to run a worker shard, and
//! its own shard has one. The home's thread cancels every task of its home at the close and as
//! it exits, waiting for a wake still on its way until the shutdown deadline; a task left then
//! is said and counted (`Stats::tasks_left`), its future leaked, for it may not be dropped on
//! another thread (proofs/tla/teardown/Teardown.tla).

use core::task::{Context, Poll};
use std::sync::Arc;
use std::time::Instant;

use super::target::Target;
use crate::rt::inbox::{Inbox, Pop};
use crate::rt::shard::backoff::Backoff;
use crate::rt::shard::{Shard, Shared};
use crate::rt::task::Runnable;
use crate::rt::task::list::{Links, TaskList};
use crate::sync::atomic_waker::AtomicWaker;

/// Where a runner's bound tasks are handed from other threads.
pub(crate) struct Home {
    /// Runnables of the home's tasks pushed by other threads; its thread
    /// alone takes from it, holding the taker's role from the home's making.
    inbox: Inbox,
    target: Target,
    /// The home's live tasks, touched on its thread only.
    tasks: TaskList,
    /// Woken as the last live task ends.
    drained: AtomicWaker,
}

impl Home {
    /// A home whose tasks run on the shard of `target` until it moves.
    pub(crate) fn new(target: Arc<Shared>) -> Self {
        Self {
            inbox: Inbox::new(),
            target: Target::new(target),
            tasks: TaskList::new(),
            drained: AtomicWaker::new(),
        }
    }

    /// Hands a runnable of one of the home's tasks over from another thread,
    /// and wakes the shard its thread runs them on.
    pub(crate) fn push(&self, runnable: Runnable) {
        // Sequentially consistent, as the target's read below: either the
        // thread moving the target finds this push, or this wakes the shard
        // it moved to.
        self.inbox.push(runnable);
        self.target.get().unpark();
    }

    /// The next runnable pushed to the home, if one is ready.
    ///
    /// # Safety
    ///
    /// The caller is the home's thread.
    pub(crate) unsafe fn pop(&self) -> Option<Runnable> {
        // SAFETY: the caller's contract: the taker's role is the thread's.
        match unsafe { self.inbox.pop() } {
            Pop::Item(runnable) => Some(runnable),
            Pop::Empty | Pop::Busy => None,
        }
    }

    /// Whether nothing pushed to the home waits, as its taker sees it.
    ///
    /// # Safety
    ///
    /// The caller is the home's thread.
    pub(crate) unsafe fn is_drained(&self) -> bool {
        // SAFETY: the caller's contract.
        unsafe { self.inbox.is_drained() }
    }

    /// The shard the home's tasks are woken on.
    pub(crate) fn target(&self) -> &Target {
        &self.target
    }

    /// Lists a task of the home.
    ///
    /// # Safety
    ///
    /// The caller is the home's thread, and the task is on no list.
    #[inline(always)]
    pub(crate) unsafe fn push_task(&self, task: &Links) {
        // SAFETY: the caller's contract.
        unsafe { self.tasks.push(task) }
    }

    /// A task of the home ended: its future was dropped and it left the list.
    ///
    /// # Safety
    ///
    /// The caller is the home's thread.
    #[inline(always)]
    pub(crate) unsafe fn task_ended(&self) {
        // SAFETY: the caller's contract.
        if unsafe { self.tasks.is_empty() } {
            self.drained.wake();
        }
    }

    /// Whether the home has no live task.
    ///
    /// # Safety
    ///
    /// The caller is the home's thread.
    pub(crate) unsafe fn is_idle(&self) -> bool {
        // SAFETY: the caller's contract.
        unsafe { self.tasks.is_empty() }
    }

    /// Ready once the home has no live task.
    ///
    /// # Safety
    ///
    /// The caller is the home's thread.
    pub(crate) unsafe fn poll_drained(&self, cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: the caller's contract.
        if unsafe { self.tasks.is_empty() } {
            return Poll::Ready(());
        }
        self.drained.register(cx.waker());
        // SAFETY: as above.
        if unsafe { self.tasks.is_empty() } {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Home {
    /// Cancels the home's tasks that are idle, on the home's thread (see
    /// [`TaskList::cancel_all`]). Returns how many it dropped.
    ///
    /// # Safety
    ///
    /// The caller is the home's thread.
    pub(crate) unsafe fn cancel_listed(&self) -> usize {
        // SAFETY: the caller's contract.
        unsafe { self.tasks.cancel_all() }
    }

    /// Ends every task of the home on its thread, as the thread is done with the home: the
    /// runnables pushed to it, and in `own`'s queues, are dropped, the idle tasks cancelled, until
    /// none is listed; a task whose wake is still on its way is waited for until `deadline`, after
    /// which it is left, said once and counted.
    ///
    /// # Safety
    ///
    /// The caller is the home's thread.
    pub(crate) unsafe fn cancel_all(&self, own: Option<&Shard>, deadline: Instant) {
        let mut backoff = Backoff::new();
        loop {
            // SAFETY: the caller's contract.
            while let Some(runnable) = unsafe { self.pop() } {
                drop(runnable);
            }
            if let Some(own) = own {
                own.drop_queued();
            }
            // SAFETY: as above.
            unsafe { self.tasks.cancel_all() };
            // SAFETY: as above.
            if unsafe { self.is_idle() } {
                return;
            }
            if Instant::now() >= deadline {
                // SAFETY: as above.
                let left = unsafe { self.tasks.len() };
                crate::rt::counters::tasks_left(left as u64);
                log::warn!(
                    "{left} tasks bound to an exiting thread were still being woken at the shutdown deadline; they are leaked"
                );
                return;
            }
            backoff.pause();
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // SAFETY: the home is gone, so nothing else reaches its inbox. Every task of the home
        // holds it, and its thread ended each before it went (a task left at the deadline is
        // leaked, and holds the home for good), so the inbox holds nothing.
        unsafe { self.inbox.drain(drop) };
    }
}
