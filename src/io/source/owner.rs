//! The shard that claimed a side of a source ([`Owner`]), kept for a nudge to reach: set by the
//! side's first claim, read by any thread after, let go of with the source.

use core::ptr;
use std::sync::Arc;

use crate::rt::shard::Shared;
use crate::sync::internal::{AtomicPtr, Ordering, take_ptr};

/// A shard's `Shared`, set once: one word, and no lock. A set that finds a shard kept already
/// drops its own count.
pub(crate) struct Owner(AtomicPtr<Shared>);

impl Owner {
    pub(crate) fn new() -> Self {
        Self(AtomicPtr::new(ptr::null_mut()))
    }

    /// Whether a shard is kept.
    pub(crate) fn is_set(&self) -> bool {
        !self.0.load(Ordering::Relaxed).is_null()
    }

    /// Keeps `shard`, unless one is kept already.
    pub(crate) fn set(&self, shard: Arc<Shared>) {
        let raw = Arc::into_raw(shard).cast_mut();
        // Release: a reader that loads the pointer finds the `Shared` it names.
        if self
            .0
            .compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // SAFETY: `raw` is the count `into_raw` gave up above, never published.
            drop(unsafe { Arc::from_raw(raw) });
        }
    }

    /// The shard kept, if any.
    pub(crate) fn get(&self) -> Option<&Shared> {
        // SAFETY: a published pointer holds the count `set` gave up, which only the drop lets go
        // of, and the drop takes `&mut self`.
        unsafe { self.0.load(Ordering::Acquire).as_ref() }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let raw = take_ptr(&mut self.0);
        if !raw.is_null() {
            // SAFETY: the count `set` published, let go of once, here.
            drop(unsafe { Arc::from_raw(raw) });
        }
    }
}
