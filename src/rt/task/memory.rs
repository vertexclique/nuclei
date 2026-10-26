//! Task memory: each task is one block, taken from this thread's cache of
//! blocks of its size when one is there, and given to the cache of the
//! thread that frees it.
//!
//! Blocks come in classes of [`STEP`] bytes up to [`CLASSES`] steps, all
//! aligned to [`STEP`]; a task larger than that, or more aligned, goes to
//! the global allocator as it is. A thread keeps at most [`KEPT`] blocks
//! of a class, so the memory a cache holds is bounded; it frees them as the
//! thread ends. In steady state a spawn allocates nothing.
//!
//! A block is freed only when nothing refers to its task any more (its last
//! waker, handle and run gone), so the next task to have it meets nothing
//! of the last one's. Under loom the cache is left out: the models check
//! the task's states, not where its memory comes from.

use core::alloc::Layout;
use core::ptr::NonNull;

/// The size step of a class, and the alignment of every cached block.
pub(crate) const STEP: usize = 64;
/// Classes cached: blocks up to `STEP * CLASSES` bytes.
pub(crate) const CLASSES: usize = 16;
/// Blocks a thread keeps of one class.
pub(crate) const KEPT: usize = 32;

/// The class of a task of `layout`, if it is cached.
fn class_of(layout: Layout) -> Option<usize> {
    (layout.align() <= STEP && layout.size() <= STEP * CLASSES && layout.size() > 0)
        .then(|| layout.size().div_ceil(STEP) - 1)
}

/// The layout every block of `class` has.
fn block(class: usize) -> Layout {
    // SAFETY: a nonzero multiple of a power of two, aligned to it.
    unsafe { Layout::from_size_align_unchecked((class + 1) * STEP, STEP) }
}

#[cfg(not(loom))]
mod cache {
    use core::cell::RefCell;
    use core::ptr::NonNull;

    use super::{CLASSES, KEPT, block};

    /// This thread's free blocks, by class.
    pub(super) struct Cache([Vec<NonNull<u8>>; CLASSES]);

    impl Drop for Cache {
        fn drop(&mut self) {
            for (class, blocks) in self.0.iter_mut().enumerate() {
                for ptr in blocks.drain(..) {
                    // SAFETY: each block was allocated with its class's layout
                    // and is no task's.
                    unsafe { std::alloc::dealloc(ptr.as_ptr(), block(class)) };
                }
            }
        }
    }

    thread_local! {
        static CACHE: RefCell<Cache> = const {
            RefCell::new(Cache([const { Vec::new() }; CLASSES]))
        };
    }

    /// A free block of `class`, if this thread keeps one.
    pub(super) fn take(class: usize) -> Option<NonNull<u8>> {
        CACHE
            .try_with(|cache| cache.try_borrow_mut().ok()?.0[class].pop())
            .ok()
            .flatten()
    }

    /// Keeps `ptr`, a block of `class`; `false` if the cache is full or gone.
    pub(super) fn keep(class: usize, ptr: NonNull<u8>) -> bool {
        CACHE
            .try_with(|cache| {
                let Ok(mut cache) = cache.try_borrow_mut() else {
                    return false;
                };
                let blocks = &mut cache.0[class];
                if blocks.len() >= KEPT {
                    return false;
                }
                // The list's own room is made once, at its full size.
                if blocks.capacity() == 0 {
                    blocks.reserve_exact(KEPT);
                }
                blocks.push(ptr);
                true
            })
            .unwrap_or(false)
    }
}

#[cfg(loom)]
mod cache {
    use core::ptr::NonNull;

    pub(super) fn take(_class: usize) -> Option<NonNull<u8>> {
        None
    }

    pub(super) fn keep(_class: usize, _ptr: NonNull<u8>) -> bool {
        false
    }
}

/// Memory for a task of `layout`.
pub(crate) fn alloc(layout: Layout) -> NonNull<u8> {
    let layout = match class_of(layout) {
        Some(class) => match cache::take(class) {
            Some(ptr) => return ptr,
            None => block(class),
        },
        None => layout,
    };
    // SAFETY: the layout is nonzero in size (a task holds its header).
    let ptr = unsafe { std::alloc::alloc(layout) };
    NonNull::new(ptr).unwrap_or_else(|| std::alloc::handle_alloc_error(layout))
}

/// Frees the memory of a task of `layout` at `ptr`, which [`alloc`] gave.
///
/// # Safety
///
/// `ptr` came from `alloc(layout)`, and nothing uses it any more.
pub(crate) unsafe fn free(ptr: NonNull<u8>, layout: Layout) {
    match class_of(layout) {
        Some(class) => {
            if !cache::keep(class, ptr) {
                // SAFETY: allocated with the class's layout.
                unsafe { std::alloc::dealloc(ptr.as_ptr(), block(class)) };
            }
        }
        // SAFETY: allocated with `layout` itself.
        None => unsafe { std::alloc::dealloc(ptr.as_ptr(), layout) },
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn classes_cover_their_sizes_and_alignments() {
        for size in 1..=STEP * CLASSES {
            let layout = Layout::from_size_align(size, 8).unwrap();
            let class = class_of(layout).unwrap();
            assert!(block(class).size() >= size);
            assert!(block(class).size() - size < STEP);
        }
        assert_eq!(
            class_of(Layout::from_size_align(STEP * CLASSES + 1, 8).unwrap()),
            None
        );
        assert_eq!(class_of(Layout::from_size_align(64, 128).unwrap()), None);
    }

    #[test]
    fn a_freed_block_is_the_next_one_of_its_class() {
        let layout = Layout::from_size_align(200, 16).unwrap();
        let a = alloc(layout);
        // SAFETY: just allocated, unused.
        unsafe { free(a, layout) };
        let b = alloc(Layout::from_size_align(250, 8).unwrap());
        assert_eq!(a, b, "the same class, the same block");
        // SAFETY: as above.
        unsafe { free(b, Layout::from_size_align(250, 8).unwrap()) };
        let big = Layout::from_size_align(STEP * CLASSES * 2, 8).unwrap();
        let c = alloc(big);
        // SAFETY: as above.
        unsafe { free(c, big) };
    }
}
