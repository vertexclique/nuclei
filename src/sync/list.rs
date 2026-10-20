//! The FIFO of waiters a drain pass keeps between passes.
//!
//! Only the holder of a primitive's drain role touches its lists, so a list
//! is a plain singly linked queue threaded through the nodes' own links.
//! Cancelled waiters stay linked until a pass meets them at the front, or
//! until enough of them pile up behind a live front that a sweep pays for
//! itself: [`List::tidy`] sweeps once the cancellations since the last
//! sweep exceed half the list, so a sweep costs at most two steps per
//! cancellation and the list never holds more than about twice its live
//! waiters.

#![allow(unsafe_code)]

use core::ptr::{self, NonNull};

use super::queue::{Arrivals, WaitQueue};
use super::waiter::{Waiter, release_queued};

pub(super) struct List {
    head: *mut Waiter,
    tail: *mut Waiter,
    len: usize,
    swept: usize,
}

impl List {
    pub(super) const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            len: 0,
            swept: 0,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    /// Nodes linked, withdrawn ones included.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn push_back(&mut self, node: NonNull<Waiter>) {
        // SAFETY: the caller hands over the queue's reference to `node`.
        unsafe { node.as_ref() }.set_next(ptr::null_mut());
        match NonNull::new(self.tail) {
            // SAFETY: the tail is a node this list holds.
            Some(tail) => unsafe { tail.as_ref() }.set_next(node.as_ptr()),
            None => self.head = node.as_ptr(),
        }
        self.tail = node.as_ptr();
        self.len += 1;
    }

    /// Appends every arrival still waiting, in arrival order, and lets go
    /// of the ones that already withdrew.
    pub(super) fn absorb(&mut self, arrivals: Arrivals, queue: &WaitQueue) {
        for node in arrivals {
            // SAFETY: the arrival stack handed over the queue's reference.
            if unsafe { node.as_ref() }.is_cancelled() {
                release_queued(node, queue);
            } else {
                self.push_back(node);
            }
        }
    }

    /// Takes the front node off the list, with the queue's reference.
    pub(super) fn pop_front(&mut self) -> Option<NonNull<Waiter>> {
        let node = NonNull::new(self.head)?;
        // SAFETY: the head is a node this list holds.
        self.head = unsafe { node.as_ref() }.next();
        if self.head.is_null() {
            self.tail = ptr::null_mut();
        }
        self.len -= 1;
        Some(node)
    }

    /// The oldest waiter still waiting, after letting go of every
    /// withdrawn one ahead of it.
    pub(super) fn front(&mut self, queue: &WaitQueue) -> Option<NonNull<Waiter>> {
        while let Some(node) = NonNull::new(self.head) {
            // SAFETY: the head is a node this list holds.
            if !unsafe { node.as_ref() }.is_cancelled() {
                return Some(node);
            }
            self.pop_front();
            release_queued(node, queue);
        }
        None
    }

    /// The waiters still waiting, oldest first, left in place. A node
    /// that withdraws during the walk is still yielded; touching it is
    /// harmless, since a withdrawn node ignores grants and nudges.
    pub(super) fn iter(&self) -> impl Iterator<Item = NonNull<Waiter>> + '_ {
        let mut cursor = self.head;
        core::iter::from_fn(move || {
            loop {
                let node = NonNull::new(cursor)?;
                // SAFETY: `cursor` walks nodes this list holds.
                let waiter = unsafe { node.as_ref() };
                cursor = waiter.next();
                if !waiter.is_cancelled() {
                    return Some(node);
                }
            }
        })
    }

    /// Sweeps out withdrawn waiters once enough have piled up (module
    /// docs).
    pub(super) fn tidy(&mut self, queue: &WaitQueue) {
        let cancels = queue.cancels();
        if cancels.wrapping_sub(self.swept) > self.len / 2 {
            self.swept = cancels;
            self.sweep(queue);
        }
    }

    fn sweep(&mut self, queue: &WaitQueue) {
        let mut prev: *mut Waiter = ptr::null_mut();
        let mut cursor = self.head;
        while let Some(node) = NonNull::new(cursor) {
            // SAFETY: `cursor` walks nodes this list holds.
            let waiter = unsafe { node.as_ref() };
            cursor = waiter.next();
            if !waiter.is_cancelled() {
                prev = node.as_ptr();
                continue;
            }
            match NonNull::new(prev) {
                // SAFETY: `prev` is a node this list holds.
                Some(prev) => unsafe { prev.as_ref() }.set_next(cursor),
                None => self.head = cursor,
            }
            if cursor.is_null() {
                self.tail = prev;
            }
            self.len -= 1;
            release_queued(node, queue);
        }
    }

    /// Lets go of every node; used when the primitive is dropped, when no
    /// future can still hold one.
    pub(super) fn clear(&mut self, queue: &WaitQueue) {
        while let Some(node) = self.pop_front() {
            release_queued(node, queue);
        }
    }
}
