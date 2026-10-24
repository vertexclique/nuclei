//! The message queue behind a channel: kovan's lock-free segment queue, kept
//! behind one small type so a model-checking build can swap it.
//!
//! The channel pushes from any number of senders and takes values from one
//! receiver (and from the receiver's drop), so the queue is used as a
//! multi-producer, single-consumer queue. The protocol the channel relies on
//! is small:
//!
//! - Values come out in the order their pushes claimed cells. A push that
//!   returned before another began claimed the earlier cell, so one sender's
//!   values come out in the order it sent them.
//! - A `push` that has returned is visible to every `try_pop` that starts
//!   after it.
//! - A value is taken at most once, and a value still queued when the queue
//!   is dropped is dropped exactly once.
//! - [`try_pop`](Queue::try_pop) never waits. When the next cell is claimed
//!   by a push that has not written its value yet, it answers
//!   [`TryPopError::Busy`] and takes nothing, so the receiver registers its
//!   waker and returns `Pending`: the pusher wakes it after its push returns,
//!   as it does for every push.
//!
//! # Under loom
//!
//! kovan's queue is not instrumented, so loom cannot interleave its steps.
//! Under `--cfg loom` the same type is a loom `Mutex` around a deque of
//! cells, and a push is two steps, as in the segment queue: it claims the
//! next cell, then writes its value into it. So a receiver can find the next
//! cell claimed and empty, and the models check the handshake the channel
//! builds around that answer (the waker, the counts, the close bit), and
//! count how often their search met it. The same split is made for the
//! cancellation token's registry (see `cancellation/rc.rs`).

pub(super) use kovan_queue::seg_queue::TryPopError;

/// The queue of one channel.
#[cfg(not(loom))]
pub(super) struct Queue<T>(crate::sync::SegQueue<T>);

/// The queue of one channel, modelled for loom.
#[cfg(loom)]
pub(super) struct Queue<T>(loom::sync::Mutex<Cells<T>>);

/// The loom model's cells: the claimed ones from the oldest on, `None` while
/// the push that claimed it has not written it yet.
#[cfg(loom)]
pub(super) struct Cells<T> {
    /// How many cells were taken: the position of the deque's front.
    taken: u64,
    cells: std::collections::VecDeque<Option<T>>,
}

#[cfg(not(loom))]
impl<T> Queue<T> {
    pub(super) fn new() -> Self {
        Self(crate::sync::SegQueue::new())
    }

    /// Appends `value`. Lock-free: it retries only when another push linked
    /// the next segment first.
    pub(super) fn push(&self, value: T) {
        self.0.push(value);
    }

    /// Takes the oldest value, without waiting: `Empty` when there is none,
    /// `Busy` when the next one is still being written.
    pub(super) fn try_pop(&self) -> Result<T, TryPopError> {
        self.0.try_pop()
    }

    /// How many values are queued, as a snapshot: a push in progress counts,
    /// a pop in progress does not.
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(loom)]
impl<T> Queue<T> {
    pub(super) fn new() -> Self {
        Self(loom::sync::Mutex::new(Cells {
            taken: 0,
            cells: std::collections::VecDeque::new(),
        }))
    }

    /// Claims the next cell, then writes it, in two steps.
    pub(super) fn push(&self, value: T) {
        let at = {
            let mut q = self.lock();
            q.cells.push_back(None);
            q.taken + q.cells.len() as u64 - 1
        };
        let mut q = self.lock();
        // Only a written cell is taken, so the claimed one is still here.
        let index = (at - q.taken) as usize;
        q.cells[index] = Some(value);
    }

    pub(super) fn try_pop(&self) -> Result<T, TryPopError> {
        let mut q = self.lock();
        let found = match q.cells.front_mut() {
            None => Err(TryPopError::Empty),
            Some(cell) => cell.take().ok_or(TryPopError::Busy),
        };
        match found {
            Ok(_) => {
                q.cells.pop_front();
                q.taken += 1;
            }
            Err(TryPopError::Busy) => crate::sync::loom_model::busy_look(),
            Err(TryPopError::Empty) => {}
        }
        found
    }

    pub(super) fn len(&self) -> usize {
        self.lock().cells.len()
    }

    fn lock(&self) -> loom::sync::MutexGuard<'_, Cells<T>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn values_of_one_pusher_come_out_in_order() {
        let queue = Queue::new();
        for i in 0..100 {
            queue.push(i);
        }
        assert_eq!(queue.len(), 100);
        for i in 0..100 {
            assert_eq!(queue.try_pop(), Ok(i));
        }
        assert_eq!(queue.try_pop(), Err(TryPopError::Empty));
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn a_value_left_in_the_queue_is_dropped_once_with_it() {
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let queue = Queue::new();
        for _ in 0..70 {
            queue.push(Counted(Arc::clone(&drops)));
        }
        assert!(queue.try_pop().is_ok());
        assert!(queue.try_pop().is_ok());
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        drop(queue);
        assert_eq!(drops.load(Ordering::SeqCst), 70);
    }
}
