//! The home of a runner's bound tasks, the ones `spawn_local` keeps on the
//! thread that spawned them.
//!
//! A bound task stays with its thread, whichever shard the thread runs: its
//! runnables pushed from other threads wait in the home's inbox, which only
//! the home's thread takes from, on whatever shard it runs. The home names
//! the shard to wake (its [`Target`]): the worker shard its thread runs,
//! until that shard is taken from the thread, then the thread's own. A
//! thread makes a new home each time it starts to run a worker shard, and
//! its own shard has one. A runnable pushed to a home whose thread is gone
//! is leaked, not dropped: its future may not be dropped on another thread.

use core::task::{Context, Poll};
use std::sync::Arc;

use super::target::Target;
use crate::rt::inbox::{Inbox, Pop};
use crate::rt::shard::Shared;
use crate::rt::task::Runnable;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicUsize, Ordering};

/// Where a runner's bound tasks are handed from other threads.
pub(crate) struct Home {
    /// Runnables of the home's tasks pushed by other threads; its thread
    /// alone takes from it, holding the taker's role from the home's making.
    inbox: Inbox,
    target: Target,
    /// The home's live tasks.
    live: AtomicUsize,
    /// Woken as the last live task ends.
    drained: AtomicWaker,
}

impl Home {
    /// A home whose tasks run on the shard of `target` until it moves.
    pub(crate) fn new(target: Arc<Shared>) -> Self {
        Self {
            inbox: Inbox::new(),
            target: Target::new(target),
            live: AtomicUsize::new(0),
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

    /// A task of the home begins.
    pub(crate) fn began(&self) {
        self.live.fetch_add(1, Ordering::Relaxed);
    }

    /// A task of the home ended: its future was dropped.
    pub(crate) fn ended(&self) {
        if self.live.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.drained.wake();
        }
    }

    /// Whether the home has no live task.
    pub(crate) fn is_idle(&self) -> bool {
        self.live.load(Ordering::Acquire) == 0
    }

    /// Ready once the home has no live task.
    pub(crate) fn poll_drained(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.live.load(Ordering::Acquire) == 0 {
            return Poll::Ready(());
        }
        self.drained.register(cx.waker());
        if self.live.load(Ordering::Acquire) == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // SAFETY: the home is gone, so nothing else reaches its inbox; what
        // it holds was pushed after its thread went.
        unsafe { self.inbox.drain(core::mem::forget) };
    }
}
