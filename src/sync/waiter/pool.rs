//! A primitive's free list of waiter nodes.

#![allow(unsafe_code)]

#[cfg(not(all(loom, test)))]
use core::ptr;
use core::ptr::NonNull;

use super::node::Waiter;
#[cfg(not(all(loom, test)))]
use crate::sync::internal::Ordering;

/// Nodes a primitive keeps for reuse; past this the allocator gets them
/// back, so a burst of contention cannot pin memory forever.
#[cfg(not(all(loom, test)))]
const POOL_CAPACITY: usize = 16;

/// A pooled node, owned by whoever holds this value.
#[cfg(not(all(loom, test)))]
struct Pooled(NonNull<Waiter>);

// SAFETY: a pooled node is referenced by nothing but its `Pooled`.
#[cfg(not(all(loom, test)))]
unsafe impl Send for Pooled {}

/// A primitive's free list of waiter nodes.
///
/// The ring is kovan's bounded lock-free MPMC queue, allocated the first
/// time a node is returned, so a primitive that never contends never
/// allocates. Under `--cfg loom` the pool is a pass-through to the
/// allocator: loom cannot see inside kovan's queue, and a hand-off it
/// cannot see would read to it as a data race.
pub(in crate::sync) struct Pool {
    #[cfg(not(all(loom, test)))]
    ring: core::sync::atomic::AtomicPtr<kovan_queue::array_queue::ArrayQueue<Pooled>>,
}

impl Pool {
    pub(in crate::sync) const fn new() -> Self {
        Self {
            #[cfg(not(all(loom, test)))]
            ring: core::sync::atomic::AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub(super) fn take(&self) -> NonNull<Waiter> {
        #[cfg(not(all(loom, test)))]
        {
            let ring = self.ring.load(Ordering::Acquire);
            // SAFETY: a published ring lives until the pool drops.
            if let Some(Pooled(node)) = unsafe { ring.as_ref() }.and_then(|ring| ring.pop()) {
                return node;
            }
        }
        NonNull::from(Box::leak(Box::new(Waiter::new())))
    }

    pub(super) fn put(&self, node: NonNull<Waiter>) {
        // SAFETY: every reference is gone, so the caller owns the node.
        unsafe { node.as_ref() }.clear_slots();
        #[cfg(not(all(loom, test)))]
        let node = match self.ring().push(Pooled(node)) {
            Ok(()) => return,
            Err(Pooled(node)) => node,
        };
        // SAFETY: as above, the caller owns the node outright.
        #[cfg(all(loom, test))]
        unsafe { &mut *node.as_ptr() }.retire();
        // SAFETY: every node is a leaked `Box` and this one is unreferenced.
        drop(unsafe { Box::from_raw(node.as_ptr()) });
    }

    #[cfg(not(all(loom, test)))]
    fn ring(&self) -> &kovan_queue::array_queue::ArrayQueue<Pooled> {
        let ring = self.ring.load(Ordering::Acquire);
        // SAFETY: a published ring lives until the pool drops.
        if let Some(ring) = unsafe { ring.as_ref() } {
            return ring;
        }
        let fresh = Box::into_raw(Box::new(kovan_queue::array_queue::ArrayQueue::new(
            POOL_CAPACITY,
        )));
        match self.ring.compare_exchange(
            ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: just published; it lives until the pool drops.
            Ok(_) => unsafe { &*fresh },
            Err(winner) => {
                // SAFETY: `fresh` lost the race and was never shared.
                drop(unsafe { Box::from_raw(fresh) });
                // SAFETY: as above, for the winner's ring.
                unsafe { &*winner }
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        #[cfg(not(all(loom, test)))]
        {
            let ring = *self.ring.get_mut();
            if !ring.is_null() {
                // SAFETY: the pool owns its ring and is being dropped.
                let ring = unsafe { Box::from_raw(ring) };
                while let Some(Pooled(node)) = ring.pop() {
                    // SAFETY: a pooled node is an unreferenced leaked `Box`.
                    drop(unsafe { Box::from_raw(node.as_ptr()) });
                }
            }
        }
    }
}
