//! A queue with a count of what it holds ([`Counted`]): an empty look is one load of the
//! count, not a look at the queue's words.
//!
//! Each push counts its item once the push returned, so the count never exceeds the items
//! pushed whole, and a consumer pops at most the count it read and takes what it popped off the
//! count. An item whose count is not seen yet stays for the consumer's next take, as a push under
//! way does in the queue itself (proofs/tla/counted/Counted.tla).

#[cfg(ring)]
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use kovan_queue::seg_queue::SegQueue;

#[cfg(ring)]
use crate::sync::internal::{AtomicPtr, take_ptr};

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

/// A [`Counted`] made at its first push: one word until then, so an owner that seldom queues
/// (a source whose arrivals mostly go straight to a free side) allocates nothing for it, and keeps
/// the queue's cache-line alignment out of its own layout. A push that loses the race to make it
/// frees its own and uses the one made.
#[cfg(ring)]
pub(crate) struct LazyCounted<T> {
    queue: AtomicPtr<Counted<T>>,
}

#[cfg(ring)]
impl<T> LazyCounted<T> {
    pub(crate) fn new() -> Self {
        Self {
            queue: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// The queue, if a push made it.
    #[inline(always)]
    fn made(&self) -> Option<&Counted<T>> {
        // SAFETY: a published queue lives until the drop, which takes `&mut self`; Acquire pairs
        // with the publishing Release, so its words are seen whole.
        unsafe { self.queue.load(Ordering::Acquire).as_ref() }
    }

    /// As [`Counted::push`], making the queue first if no push did.
    pub(crate) fn push(&self, item: T) {
        match self.made() {
            Some(queue) => queue.push(item),
            None => self.make().push(item),
        }
    }

    #[cold]
    #[inline(never)]
    fn make(&self) -> &Counted<T> {
        let fresh = Box::into_raw(Box::new(Counted::new()));
        match self.queue.compare_exchange(
            ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: published, it lives until the drop.
            Ok(_) => unsafe { &*fresh },
            Err(made) => {
                // SAFETY: `fresh` was never published; `made` was, and lives until the drop.
                unsafe {
                    drop(Box::from_raw(fresh));
                    &*made
                }
            }
        }
    }

    /// As [`Counted::is_empty`]; a queue not made yet holds nothing.
    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.made().is_none_or(Counted::is_empty)
    }

    /// As [`Counted::take_all`].
    #[inline(always)]
    pub(crate) fn take_all(&self, take: impl FnMut(T)) {
        if let Some(queue) = self.made() {
            queue.take_all(take);
        }
    }
}

#[cfg(ring)]
impl<T> Drop for LazyCounted<T> {
    fn drop(&mut self) {
        let queue = take_ptr(&mut self.queue);
        if !queue.is_null() {
            // SAFETY: the box `make` published, let go of once, here.
            drop(unsafe { Box::from_raw(queue) });
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
#[cfg(all(test, ring, not(loom)))]
mod tests_lazy;
