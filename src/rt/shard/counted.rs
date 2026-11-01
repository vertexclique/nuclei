//! A queue with a count of what it holds ([`Counted`]): an empty look is one load of the
//! count, not a look at the queue's words.
//!
//! Each push counts its item once the push returned, so the count never exceeds the items
//! pushed whole, and a consumer pops at most the count it read and takes what it popped off the
//! count. An item whose count is not seen yet stays for the consumer's next take, as a push under
//! way does in the queue itself (proofs/tla/counted/Counted.tla).

use core::sync::atomic::{AtomicUsize, Ordering};

use kovan_queue::seg_queue::SegQueue;

/// A queue of many producers and one consumer, counted (see the module docs).
pub(crate) struct Counted<T> {
    queue: SegQueue<T>,
    count: AtomicUsize,
}

impl<T> Counted<T> {
    pub(crate) fn new() -> Self {
        Self {
            queue: SegQueue::new(),
            count: AtomicUsize::new(0),
        }
    }

    /// Adds `item` after those pushed before it.
    pub(crate) fn push(&self, item: T) {
        self.queue.push(item);
        // Release: the consumer that reads the count finds the push done.
        self.count.fetch_add(1, Ordering::Release);
    }

    /// Whether no counted item waits: a look that may miss a push whose count is not in yet.
    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.count.load(Ordering::Relaxed) == 0
    }

    /// Whether the queue holds an item or a push under way, with a look at its words: for a look
    /// that must not miss a push whose count is not in yet.
    pub(crate) fn holds_any(&self) -> bool {
        !self.queue.is_empty()
    }

    /// The items the queue holds, for a test that failed.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    /// Takes the counted items in order, each into `take`. A push still under way ahead of a
    /// counted item keeps the rest for the next call. Consumers that take at once with each other
    /// (the drains of a shard that stopped) may each read a count another takes off, and the
    /// count may go below zero for a moment (it wraps), until the push whose item one took before
    /// it was counted counts it: a look then finds work that is not there, never the reverse.
    pub(crate) fn take_all(&self, mut take: impl FnMut(T)) {
        // Acquire: pairs with the count's Release, so each counted item's push is done.
        let counted = self.count.load(Ordering::Acquire);
        let mut taken = 0;
        while taken < counted {
            let Ok(item) = self.queue.try_pop() else {
                break;
            };
            taken += 1;
            take(item);
        }
        if taken > 0 {
            self.count.fetch_sub(taken, Ordering::Relaxed);
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
