//! [`BlockArc`]: a reference-counted value in a block of the thread's cache,
//! for the shared state made and dropped per operation (a oneshot
//! channel's). As `Arc`, but its memory comes from the cache and goes back
//! to the thread it came from, so in steady state it makes no call into the
//! global allocator.

use core::alloc::Layout;
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering, fence};

use super::Owner;

/// A shared value, counted; dropped with its last handle.
pub(crate) struct BlockArc<T> {
    ptr: NonNull<Block<T>>,
    owns: PhantomData<Block<T>>,
}

struct Block<T> {
    count: AtomicUsize,
    /// The thread whose cache the block came from.
    owner: Owner,
    value: T,
}

// SAFETY: as `Arc`'s: the value is shared between the handles' threads, and
// dropped on whichever thread lets go last.
unsafe impl<T: Send + Sync> Send for BlockArc<T> {}
// SAFETY: as for `Send`.
unsafe impl<T: Send + Sync> Sync for BlockArc<T> {}

// A handle is a pointer: moving it never moves the value.
impl<T> Unpin for BlockArc<T> {}

impl<T> BlockArc<T> {
    /// `value`, shared, with one handle.
    pub(crate) fn new(value: T) -> Self {
        let (ptr, owner) = super::alloc(Layout::new::<Block<T>>());
        let ptr = ptr.cast::<Block<T>>();
        // SAFETY: fresh memory of the block's layout, written once here.
        unsafe {
            ptr.as_ptr().write(Block {
                count: AtomicUsize::new(1),
                owner,
                value,
            });
        }
        Self {
            ptr,
            owns: PhantomData,
        }
    }

    fn block(&self) -> &Block<T> {
        // SAFETY: the block lives while any handle does.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> Clone for BlockArc<T> {
    fn clone(&self) -> Self {
        // Relaxed, as for any reference count: the clone holds a reference
        // already, so nothing it reads is new.
        if self.block().count.fetch_add(1, Ordering::Relaxed) > isize::MAX as usize {
            std::process::abort();
        }
        Self {
            ptr: self.ptr,
            owns: PhantomData,
        }
    }
}

impl<T> Deref for BlockArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.block().value
    }
}

impl<T> Drop for BlockArc<T> {
    fn drop(&mut self) {
        // Release: this handle's uses come before the last one's drop.
        if self.block().count.fetch_sub(1, Ordering::Release) != 1 {
            return;
        }
        // Acquire: every other handle's uses come before the value's drop.
        fence(Ordering::Acquire);
        let owner = self.block().owner;
        // SAFETY: the last handle: nothing reaches the block now. The value
        // is dropped in place, then its memory goes back where it came from.
        unsafe {
            core::ptr::drop_in_place(self.ptr.as_ptr());
            super::free(self.ptr.cast(), Layout::new::<Block<T>>(), owner);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize as Counter;

    use super::*;

    struct Counted<'a>(&'a Counter);

    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_value_is_dropped_once_with_its_last_handle() {
        let drops = Counter::new(0);
        let a = BlockArc::new(Counted(&drops));
        let b = a.clone();
        drop(a);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(b);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn handles_on_two_threads_share_one_value() {
        let a = BlockArc::new(Counter::new(0));
        let b = a.clone();
        std::thread::spawn(move || {
            b.fetch_add(1, Ordering::SeqCst);
        })
        .join()
        .unwrap();
        assert_eq!(a.load(Ordering::SeqCst), 1);
    }

    /// Dropped last on another thread, the block goes back to the thread it
    /// came from, whose next block of its size it is.
    #[test]
    fn a_block_dropped_last_elsewhere_comes_back_to_its_thread() {
        std::thread::spawn(|| {
            let a = BlockArc::new([0u64; 16]);
            let addr = core::ptr::from_ref(&*a) as usize;
            std::thread::spawn(move || drop(a)).join().unwrap();
            let b = BlockArc::new([1u64; 16]);
            assert_eq!(core::ptr::from_ref(&*b) as usize, addr);
        })
        .join()
        .unwrap();
    }
}
