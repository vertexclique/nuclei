//! The wake list a drain pass hands granted and nudged waiters to, so no
//! executor code runs while the pass holds the drain role.

#![allow(unsafe_code)]

use core::mem;
use core::ptr::{self, NonNull};
use core::task::Waker;

use super::node::{GRANTED, NUDGED, Taken, Touch, Waiter, release_queued};
use crate::sync::internal::Ordering;
use crate::sync::queue::WaitQueue;

/// Waiters a drain pass granted or nudged, woken in order once the pass
/// has given up the drain role, so no executor code runs while it holds
/// it.
pub(in crate::sync) struct WakeList<'a> {
    head: *mut Waiter,
    tail: *mut Waiter,
    queue: &'a WaitQueue,
}

impl<'a> WakeList<'a> {
    pub(in crate::sync) fn new(queue: &'a WaitQueue) -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            queue,
        }
    }

    /// Grants `node`, which the caller took off its waiting list with the
    /// queue's reference. Returns false when the future had withdrawn, in
    /// which case the caller gives back whatever it set aside for it.
    pub(in crate::sync) fn grant(&mut self, node: NonNull<Waiter>) -> bool {
        // SAFETY: the caller holds the queue's reference.
        let touch = unsafe { node.as_ref() }.touch(GRANTED, false);
        if let Touch::Claimed = touch {
            self.push(node);
        }
        release_queued(node, self.queue);
        !matches!(touch, Touch::Skipped)
    }

    /// Nudges `node`, which stays on its waiting list: it is woken to try
    /// again. Returns whether a nudge is on its way to it, this one or an
    /// earlier one its future has not looked at yet. An earlier one counts:
    /// a caller that took it for "no nudge" would lower the marker of a
    /// nudge that is still out, and every release would then ask for a pass
    /// until that waiter polls, which a waiter that holds the drain role
    /// itself cannot do while those passes keep coming.
    pub(in crate::sync) fn nudge(&mut self, node: NonNull<Waiter>) -> bool {
        // SAFETY: the caller's list holds the queue's reference.
        match unsafe { node.as_ref() }.touch(NUDGED, true) {
            Touch::Claimed => {
                self.push(node);
                true
            }
            Touch::Marked | Touch::Pending => true,
            Touch::Skipped => false,
        }
    }

    fn push(&mut self, node: NonNull<Waiter>) {
        // SAFETY: the claim gave this list the node's wake reference.
        unsafe { node.as_ref() }
            .wake_next
            .store(ptr::null_mut(), Ordering::Relaxed);
        match NonNull::new(self.tail) {
            // SAFETY: the tail is a node this list holds.
            Some(tail) => unsafe { tail.as_ref() }
                .wake_next
                .store(node.as_ptr(), Ordering::Relaxed),
            None => self.head = node.as_ptr(),
        }
        self.tail = node.as_ptr();
    }

    fn pop(&mut self) -> Option<Option<Waker>> {
        let node = NonNull::new(self.head)?;
        // SAFETY: this list holds the node's wake reference.
        let waiter = unsafe { node.as_ref() };
        // The link is read while this list still holds the node. The take
        // below may let go of the last reference but one, and the party
        // holding that one may then free or reuse the node at once. Nobody
        // else writes the link meanwhile: the taking bit keeps every other
        // wake list from claiming the node until the take lets it go.
        let next = waiter.wake_next.load(Ordering::Relaxed);
        let (waker, taken) = waiter.take_claimed();
        match taken {
            // The node stays at the front for the waker just claimed.
            Taken::Again => {}
            Taken::Done(last) => {
                self.head = next;
                if self.head.is_null() {
                    self.tail = ptr::null_mut();
                }
                if last {
                    self.queue.pool().put(node);
                }
            }
        }
        Some(waker)
    }
}

impl Drop for WakeList<'_> {
    fn drop(&mut self) {
        while let Some(waker) = self.pop() {
            if let Some(waker) = waker {
                // A waker that panics must not strand the waiters behind
                // it: they already own what they were granted, or must
                // still be told to try again.
                let rest = WakeRest(self);
                waker.wake();
                mem::forget(rest);
            }
        }
    }
}

struct WakeRest<'b, 'a>(&'b mut WakeList<'a>);

impl Drop for WakeRest<'_, '_> {
    fn drop(&mut self) {
        while let Some(waker) = self.0.pop() {
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}
