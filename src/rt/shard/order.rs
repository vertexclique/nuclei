//! The order a shard takes its runnables in (see the module docs of
//! `shard`): the LIFO slot, the local queue, then the work other threads
//! handed it, the inbox's and the home's.
//!
//! Every `tuning.inbox_interval` runs the remote work comes first, so a busy
//! local queue does not starve it. The home is looked at only once the
//! inbox is empty, so other threads that keep the inbox full hold the bound
//! tasks off: `proofs/tla/sched/Sched.tla` checks the order, today's as its
//! `home_after_empty` mutation, which breaks `Served`.

use super::Shard;
use crate::rt::inbox::Pop;
use crate::rt::task::Runnable;

impl Shard {
    /// The next runnable, in the loop's order.
    #[inline(always)]
    pub(super) fn next(&self) -> Option<Runnable> {
        let left = self.until_inbox.get().wrapping_sub(1);
        if left == 0 {
            self.until_inbox.set(self.tuning.inbox_interval);
            if let Some(r) = self.pop_inbox() {
                return Some(r);
            }
        } else {
            self.until_inbox.set(left);
        }
        if let Some(r) = self.lifo.take() {
            if self.lifo_streak.get() < super::LIFO_MAX {
                self.lifo_streak.set(self.lifo_streak.get() + 1);
                return Some(r);
            }
            // The slot's task ran enough times in a row: to the back, which
            // is the front when nothing else is queued.
            let mut queue = self.queue.borrow_mut();
            if queue.is_empty() {
                self.lifo_streak.set(0);
                return Some(r);
            }
            queue.push_back(r);
        }
        self.lifo_streak.set(0);
        let local = self.queue.borrow_mut().pop_front();
        // A match, not `or_else`: a closure's call can stay out of line, which every run that
        // finds the local queue empty would pay.
        match local {
            Some(r) => Some(r),
            None => self.pop_inbox(),
        }
    }

    // Inlined into the run loop's `next`, whichever codegen unit holds it: it is looked at on
    // every run that finds the local queue empty.
    #[inline(always)]
    pub(super) fn pop_inbox(&self) -> Option<Runnable> {
        // SAFETY: the shard holds the inbox's taker's role until it goes
        // (`stop` lets go of it, last).
        let popped = match unsafe { self.shared.inbox.pop() } {
            Pop::Item(runnable) => Some(runnable),
            // A push under way counts as work: the loop comes back. Then the
            // bound tasks of the thread that runs the shard.
            Pop::Empty | Pop::Busy => self.pop_home(),
        };
        let runnable = popped?;
        self.remote_wakes.set(self.remote_wakes.get() + 1);
        Some(runnable)
    }

    /// A bound task of the thread that runs the shard: a runner's, or the
    /// ones of this shard, a thread's own.
    #[inline(always)]
    fn pop_home(&self) -> Option<Runnable> {
        crate::rt::context::pop_home().or_else(|| {
            // SAFETY: an own shard runs on its thread only, its home's.
            self.home.get().and_then(|home| unsafe { home.pop() })
        })
    }
}
