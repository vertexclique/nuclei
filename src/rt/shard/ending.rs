//! The end of a worker shard's tasks, on its runner's thread, as the shard stops: what its queues
//! and inbox hold is dropped, and each task of its list cancelled, before the runner lets go of
//! the shard (proofs/tla/teardown/Teardown.tla, `EndShard`).

use super::Shard;

impl Shard {
    /// Drops the runnables in the LIFO slot and the local queue, on this thread. Returns how many.
    pub(crate) fn drop_queued(&self) -> usize {
        let lifo = self.lifo.take();
        let queue = core::mem::take(&mut *self.queue.borrow_mut());
        usize::from(lifo.is_some()) + queue.len()
    }

    /// For the shard's runner as the shard stops: drops what the queues and the inbox hold (the
    /// bound tasks of this thread's home too) and cancels the tasks of the shard's list, until a
    /// pass drops nothing. A task it only closed stays listed for the shard's going.
    pub(crate) fn end_tasks(&self) {
        loop {
            let mut dropped = self.drop_queued();
            while let Some(runnable) = self.pop_inbox() {
                drop(runnable);
                dropped += 1;
            }
            self.apply_list_ops();
            // SAFETY: this thread runs the shard, the list's owner.
            dropped += unsafe { self.shared.tasks.cancel_all() };
            if dropped == 0 {
                return;
            }
        }
    }
}
