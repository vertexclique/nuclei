//! [`BlockRc`]: a reference-counted value in a block of the thread's cache,
//! for the state a `sync::local` primitive makes and drops per operation (a
//! one-shot channel's). As `Rc`: its handles stay on one thread, so its
//! memory goes back to that thread's cache, and in steady state it makes no
//! call into the global allocator.

use core::alloc::Layout;
use core::cell::Cell;
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;

use super::Owner;

#[cfg(all(test, not(loom)))]
mod tests;

/// A shared value, counted; dropped with its last handle. `!Send` and
/// `!Sync`, as its count is a plain cell.
pub(crate) struct BlockRc<T> {
    ptr: NonNull<Block<T>>,
    owns: PhantomData<Block<T>>,
}

struct Block<T> {
    count: Cell<usize>,
    /// The thread whose cache the block came from: this one.
    owner: Owner,
    value: T,
}

// A handle is a pointer: moving it never moves the value.
impl<T> Unpin for BlockRc<T> {}

impl<T> BlockRc<T> {
    /// `value`, shared, with one handle.
    #[inline]
    pub(crate) fn new(value: T) -> Self {
        let (ptr, owner) = super::alloc(Layout::new::<Block<T>>());
        let ptr = ptr.cast::<Block<T>>();
        // SAFETY: fresh memory of the block's layout, written once here.
        unsafe {
            ptr.as_ptr().write(Block {
                count: Cell::new(1),
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

impl<T> Clone for BlockRc<T> {
    #[inline]
    fn clone(&self) -> Self {
        let count = self.block().count.get();
        if count == usize::MAX {
            std::process::abort();
        }
        self.block().count.set(count + 1);
        Self {
            ptr: self.ptr,
            owns: PhantomData,
        }
    }
}

impl<T> Deref for BlockRc<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.block().value
    }
}

impl<T> Drop for BlockRc<T> {
    #[inline]
    fn drop(&mut self) {
        let count = self.block().count.get() - 1;
        self.block().count.set(count);
        if count == 0 {
            self.drop_last();
        }
    }
}

impl<T> BlockRc<T> {
    /// The last handle's drop: the value's, then the block's.
    #[inline(never)]
    fn drop_last(&mut self) {
        let owner = self.block().owner;
        // SAFETY: the last handle: nothing reaches the block now. The value
        // is dropped in place, then its memory goes back where it came from.
        unsafe {
            core::ptr::drop_in_place(self.ptr.as_ptr());
            super::free(self.ptr.cast(), Layout::new::<Block<T>>(), owner);
        }
    }
}
