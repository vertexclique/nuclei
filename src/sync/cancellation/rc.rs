//! A model of the parts of `std::sync::{Arc, Weak}` the tree uses, for loom.
//!
//! loom's `Arc` has no weak reference and no `into_inner`, so this keeps a
//! logical strong count on a loom atomic over a physical `std` `Arc` that
//! stays allocated while any strong or weak handle exists. The value is
//! dropped by the thread that takes the strong count to zero, as with `std`,
//! and `upgrade` is a compare-and-swap that refuses a zero count.

#![allow(unsafe_code)]

use core::mem::ManuallyDrop;
use core::ops::Deref;
use loom::sync::atomic::{AtomicUsize, Ordering};

struct Cell<T> {
    strong: AtomicUsize,
    value: core::cell::UnsafeCell<ManuallyDrop<T>>,
}

// SAFETY: the value is read through shared references while the strong
// count is non-zero and dropped or moved out once, by the thread that
// took the count to zero.
unsafe impl<T: Send + Sync> Send for Cell<T> {}
// SAFETY: as above.
unsafe impl<T: Send + Sync> Sync for Cell<T> {}

pub(super) struct Arc<T> {
    cell: std::sync::Arc<Cell<T>>,
}

pub(super) struct Weak<T> {
    cell: std::sync::Arc<Cell<T>>,
}

impl<T> Arc<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            cell: std::sync::Arc::new(Cell {
                strong: AtomicUsize::new(1),
                value: core::cell::UnsafeCell::new(ManuallyDrop::new(value)),
            }),
        }
    }

    pub(super) fn downgrade(this: &Self) -> Weak<T> {
        Weak {
            cell: std::sync::Arc::clone(&this.cell),
        }
    }

    pub(super) fn ptr_eq(a: &Self, b: &Self) -> bool {
        std::sync::Arc::ptr_eq(&a.cell, &b.cell)
    }

    pub(super) fn as_ptr(this: &Self) -> *const T {
        std::sync::Arc::as_ptr(&this.cell).cast()
    }

    /// The value, if this was the last strong handle.
    pub(super) fn into_inner(this: Self) -> Option<T> {
        let this = ManuallyDrop::new(this);
        // SAFETY: `this` is never used again, so the physical handle is
        // moved out exactly once.
        let cell = unsafe { core::ptr::read(&this.cell) };
        // AcqRel: the thread that takes the count to zero sees every
        // other handle's accesses to the value.
        if cell.strong.fetch_sub(1, Ordering::AcqRel) != 1 {
            return None;
        }
        // SAFETY: the count reached zero, so no handle can reach the
        // value, and it is moved out once.
        Some(ManuallyDrop::into_inner(unsafe {
            core::ptr::read(cell.value.get())
        }))
    }
}

impl<T> Clone for Arc<T> {
    fn clone(&self) -> Self {
        self.cell.strong.fetch_add(1, Ordering::Relaxed);
        Self {
            cell: std::sync::Arc::clone(&self.cell),
        }
    }
}

impl<T> Deref for Arc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this handle holds a strong count, so the value is alive.
        unsafe { &**self.cell.value.get() }
    }
}

impl<T> Drop for Arc<T> {
    fn drop(&mut self) {
        if self.cell.strong.fetch_sub(1, Ordering::AcqRel) == 1 {
            // SAFETY: the count reached zero: no handle reaches the value.
            unsafe { ManuallyDrop::drop(&mut *self.cell.value.get()) };
        }
    }
}

impl<T> Weak<T> {
    pub(super) fn upgrade(&self) -> Option<Arc<T>> {
        let mut strong = self.cell.strong.load(Ordering::Relaxed);
        loop {
            if strong == 0 {
                return None;
            }
            match self.cell.strong.compare_exchange(
                strong,
                strong + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(Arc {
                        cell: std::sync::Arc::clone(&self.cell),
                    });
                }
                Err(actual) => strong = actual,
            }
        }
    }
}

impl<T> Clone for Weak<T> {
    fn clone(&self) -> Self {
        Self {
            cell: std::sync::Arc::clone(&self.cell),
        }
    }
}
