//! The message queue behind a channel: kovan's lock-free segment queue, kept
//! behind one small type so a model-checking build can swap it.
//!
//! The channel pushes from any number of senders and pops from one receiver
//! (and from the receiver's drop), so the queue is used as a multi-producer,
//! single-consumer queue. The protocol the channel relies on is small:
//!
//! - A `push` that has returned is visible to every `pop` that starts after it
//!   (the segment queue publishes a slot with a release store and a pop reads
//!   it with an acquire load).
//! - A value is popped at most once, and a value still queued when the queue is
//!   dropped is dropped exactly once.
//! - A `pop` that finds nothing may leave a push that is still in progress for
//!   later: the pusher wakes the receiver after its push, so the receiver
//!   looks again.
//!
//! One property the channel needs is **not** held by kovan's queue: that
//! values of one sender come out in the order that sender pushed them. A `pop`
//! scans the head segment and takes the first published slot it finds, passing
//! over a slot a pusher has claimed but not finished writing. If the popping
//! thread stalls between that look and the look at a later slot, the same
//! pusher can finish both pushes and the later value is taken first. This was
//! reproduced on the queue alone (two pushers, one popper looping on `pop`:
//! out-of-order deliveries about once in 10^5 values) and through the channel.
//! A queue that gives each push a position in claim order, and pops positions
//! in order, keeps the property; the channel needs nothing else changed.
//!
//! # Under loom
//!
//! kovan's queue is not instrumented, so loom cannot interleave its steps.
//! Under `--cfg loom` the same four-method type is a `Mutex<VecDeque<T>>` of
//! loom's: every push and pop is then one atomic step with the same protocol,
//! and the models check what the channel builds around the queue (the waker
//! handshake, the counts, the close bit). The same split is made for the
//! cancellation token's registry (see `cancellation/rc.rs`).

#[cfg(loom)]
use std::collections::VecDeque;

/// The queue of one channel.
#[cfg(not(loom))]
pub(super) struct Queue<T: 'static>(crate::sync::SegQueue<T>);

/// The queue of one channel, modelled for loom.
#[cfg(loom)]
pub(super) struct Queue<T: 'static>(loom::sync::Mutex<VecDeque<T>>);

#[cfg(not(loom))]
impl<T: 'static> Queue<T> {
    pub(super) fn new() -> Self {
        Self(crate::sync::SegQueue::new())
    }

    /// Appends `value`. Lock-free: it retries when it loses a slot to another
    /// sender, and allocates a segment when the last one is full.
    pub(super) fn push(&self, value: T) {
        self.0.push(value);
    }

    /// Takes the oldest value that has been published, if any.
    pub(super) fn pop(&self) -> Option<T> {
        self.0.pop()
    }

    /// How many values are queued, as a snapshot.
    ///
    /// The segment queue counts a push after it published the value and a pop
    /// after it took it, so while another thread is between the two steps the
    /// count is off by that thread's value, and may even dip below zero (which
    /// shows as a huge unsigned number). A dip reads as zero.
    pub(super) fn len(&self) -> usize {
        snapshot(self.0.len())
    }
}

/// A queue's own count as a length: a dip below zero reads as zero.
#[cfg(not(loom))]
fn snapshot(counted: usize) -> usize {
    if counted > isize::MAX as usize {
        0
    } else {
        counted
    }
}

#[cfg(loom)]
impl<T: 'static> Queue<T> {
    pub(super) fn new() -> Self {
        Self(loom::sync::Mutex::new(VecDeque::new()))
    }

    pub(super) fn push(&self, value: T) {
        self.lock().push_back(value);
    }

    pub(super) fn pop(&self) -> Option<T> {
        // Bound apart so the lock is released before the value is returned.
        let popped = self.lock().pop_front();
        popped
    }

    pub(super) fn len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> loom::sync::MutexGuard<'_, VecDeque<T>> {
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
            assert_eq!(queue.pop(), Some(i));
        }
        assert_eq!(queue.pop(), None);
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
        drop(queue.pop());
        drop(queue.pop());
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        drop(queue);
        assert_eq!(drops.load(Ordering::SeqCst), 70);
    }

    #[test]
    fn a_count_that_dipped_below_zero_reads_as_zero() {
        assert_eq!(snapshot(0), 0);
        assert_eq!(snapshot(7), 7);
        // A pop that overtook its push's count wraps the unsigned counter.
        assert_eq!(snapshot(0usize.wrapping_sub(1)), 0);
        assert_eq!(snapshot(isize::MAX as usize), isize::MAX as usize);
    }
}
