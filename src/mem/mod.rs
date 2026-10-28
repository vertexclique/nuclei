//! Small blocks: each task is one, and each piece of shared state made and
//! dropped per operation ([`BlockArc`], a oneshot channel's), taken from this
//! thread's cache of blocks of its size when one is there, and given back to
//! the cache of the thread it came from when it is freed.
//!
//! Blocks come in classes of [`STEP`] bytes up to [`CLASSES`] steps, all
//! aligned to [`STEP`]; a task larger than that, or more aligned, goes to
//! the global allocator as it is. A thread keeps at most [`KEPT_BYTES`] of
//! blocks of a class (many small blocks, few large ones), so the memory a
//! cache holds is bounded by bytes; it frees them as the thread ends. In
//! steady state a spawn allocates nothing.
//!
//! A block freed on another thread than its owner's (a task spawned here
//! and ended on a worker or a pool thread) goes back to its owner through
//! the owner's record: a list other threads push the blocks onto, linked
//! through the blocks themselves, bounded by [`KEPT_BYTES`], and taken
//! whole by the owner when one of its classes has no block left. A push
//! the list refuses (full, or closed because its thread ended) leaves the
//! block to the freeing thread's cache, or to the global allocator.
//! Records are never freed: one whose thread ended is closed, then claimed
//! and opened again by the next thread that allocates
//! (proofs/tla/remote).
//!
//! A block is freed only when nothing refers to its task any more (its last
//! waker, handle and run gone), so the next task to have it meets nothing
//! of the last one's. Under loom the cache is left out: the models check
//! the task's states, not where its memory comes from.

use core::alloc::Layout;
use core::ptr::NonNull;

mod arc;
pub(crate) use self::arc::BlockArc;

/// The size step of a class, and the alignment of every cached block.
pub(crate) const STEP: usize = 64;
/// Classes cached: blocks up to `STEP * CLASSES` bytes.
pub(crate) const CLASSES: usize = 16;
/// The bytes of blocks a thread keeps of one class, and its record holds.
pub(crate) const KEPT_BYTES: usize = 16 * 1024;

/// The blocks a thread keeps of `class`.
fn kept(class: usize) -> usize {
    KEPT_BYTES / block(class).size()
}

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

/// The thread a block came from: where it goes back when another thread
/// frees it. None for a block no cache gave.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Owner(*const cache::Record);

// SAFETY: a record is never freed, and is reached only through its atomics.
unsafe impl Send for Owner {}
// SAFETY: as for `Send`.
unsafe impl Sync for Owner {}

impl Owner {
    const NONE: Owner = Owner(core::ptr::null());
}

#[cfg(not(all(loom, test)))]
mod cache {
    use core::cell::RefCell;
    use core::ptr::{self, NonNull};
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

    use super::{CLASSES, KEPT_BYTES, Owner, block, kept};

    /// A block given back, as it lies in the list: the next one, and its
    /// class. A block is at least `STEP` bytes, aligned to `STEP`.
    #[repr(C)]
    struct Freed {
        next: *mut Freed,
        class: usize,
    }

    /// The head of a list whose thread ended: no block's address, since
    /// blocks are aligned to `STEP`.
    const CLOSED: *mut Freed = ptr::dangling_mut();

    /// A thread's record: the blocks other threads gave back to it.
    pub(super) struct Record {
        head: AtomicPtr<Freed>,
        /// Bytes of blocks in the list, and of pushes under way: it bounds
        /// the list.
        bytes: AtomicUsize,
        claimed: AtomicBool,
        /// The record made before this one, set before it was published.
        next: AtomicPtr<Record>,
    }

    /// Every record ever made, the last first; none is ever freed.
    static RECORDS: AtomicPtr<Record> = AtomicPtr::new(ptr::null_mut());

    /// A closed record no thread can claim (it is not in `RECORDS`).
    #[cfg(test)]
    pub(super) static CLOSED_RECORD: Record = Record {
        head: AtomicPtr::new(CLOSED),
        bytes: AtomicUsize::new(0),
        claimed: AtomicBool::new(true),
        next: AtomicPtr::new(ptr::null_mut()),
    };

    /// A record for this thread: one whose thread ended, opened again, or
    /// a new one.
    fn claim() -> &'static Record {
        let mut at = RECORDS.load(Ordering::Acquire);
        // SAFETY: records are never freed.
        while let Some(record) = unsafe { at.as_ref() } {
            if !record.claimed.load(Ordering::Relaxed)
                && record
                    .claimed
                    .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                // Its last thread closed it.
                record.head.store(ptr::null_mut(), Ordering::Relaxed);
                return record;
            }
            at = record.next.load(Ordering::Relaxed);
        }
        // Once per thread that allocates, and only while every record is
        // a live thread's.
        let record: &'static Record = Box::leak(Box::new(Record {
            head: AtomicPtr::new(ptr::null_mut()),
            bytes: AtomicUsize::new(0),
            claimed: AtomicBool::new(true),
            next: AtomicPtr::new(ptr::null_mut()),
        }));
        let mut last = RECORDS.load(Ordering::Acquire);
        loop {
            record.next.store(last, Ordering::Relaxed);
            match RECORDS.compare_exchange_weak(
                last,
                ptr::from_ref(record).cast_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return record,
                Err(now) => last = now,
            }
        }
    }

    /// Gives `ptr`, a block of `class`, back to `record`'s thread; `false`
    /// if its list is full or closed.
    fn give_back(record: &Record, ptr: NonNull<u8>, class: usize) -> bool {
        // The bytes' old value decides; giving up undoes the count.
        let size = block(class).size();
        if record.bytes.fetch_add(size, Ordering::Relaxed) + size > KEPT_BYTES {
            record.bytes.fetch_sub(size, Ordering::Relaxed);
            return false;
        }
        let freed = ptr.cast::<Freed>().as_ptr();
        let mut head = record.head.load(Ordering::Relaxed);
        loop {
            if head == CLOSED {
                record.bytes.fetch_sub(size, Ordering::Relaxed);
                return false;
            }
            // SAFETY: the block is no task's and holds a `Freed`; it is
            // this thread's until the exchange publishes it.
            unsafe { freed.write(Freed { next: head, class }) };
            match record.head.compare_exchange_weak(
                head,
                freed,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => head = now,
            }
        }
    }

    /// This thread's free blocks, by class, and its record once it
    /// allocated.
    pub(super) struct Cache {
        lists: [Vec<NonNull<u8>>; CLASSES],
        record: Option<&'static Record>,
    }

    impl Cache {
        /// Keeps `ptr`, a block of `class`; `false` if the list is full.
        fn keep(&mut self, class: usize, ptr: NonNull<u8>) -> bool {
            let blocks = &mut self.lists[class];
            if blocks.len() >= kept(class) {
                return false;
            }
            // The list's own room is made once, at its full size.
            if blocks.capacity() == 0 {
                blocks.reserve_exact(kept(class));
            }
            blocks.push(ptr);
            true
        }

        /// Takes the blocks given back to `record` (this thread's): kept,
        /// or freed when their class is full.
        fn take_back(&mut self, record: &Record) {
            if record.head.load(Ordering::Relaxed).is_null() {
                return;
            }
            let mut list = record.head.swap(ptr::null_mut(), Ordering::Acquire);
            let mut taken = 0;
            while !list.is_null() {
                // SAFETY: a block given back, this thread's since the swap.
                let Freed { next, class } = unsafe { list.read() };
                // SAFETY: as above; a block is never null.
                let ptr = unsafe { NonNull::new_unchecked(list.cast::<u8>()) };
                if !self.keep(class, ptr) {
                    // SAFETY: allocated with its class's layout, no task's.
                    unsafe { std::alloc::dealloc(ptr.as_ptr(), block(class)) };
                }
                taken += block(class).size();
                list = next;
            }
            record.bytes.fetch_sub(taken, Ordering::Relaxed);
        }
    }

    impl Drop for Cache {
        fn drop(&mut self) {
            // The list is closed and emptied before the record goes, so no
            // block is left in it (`OpenWhileOwned`, `AllFreed`).
            if let Some(record) = self.record {
                let mut list = record.head.swap(CLOSED, Ordering::Acquire);
                let mut taken = 0;
                while !list.is_null() {
                    // SAFETY: a block given back, this thread's since the swap.
                    let Freed { next, class } = unsafe { list.read() };
                    // SAFETY: allocated with its class's layout, no task's.
                    unsafe { std::alloc::dealloc(list.cast(), block(class)) };
                    taken += block(class).size();
                    list = next;
                }
                record.bytes.fetch_sub(taken, Ordering::Relaxed);
            }
            for (class, blocks) in self.lists.iter_mut().enumerate() {
                for ptr in blocks.drain(..) {
                    // SAFETY: each block was allocated with its class's layout
                    // and is no task's.
                    unsafe { std::alloc::dealloc(ptr.as_ptr(), block(class)) };
                }
            }
            if let Some(record) = self.record {
                record.claimed.store(false, Ordering::Release);
            }
        }
    }

    thread_local! {
        static CACHE: RefCell<Cache> = const {
            RefCell::new(Cache {
                lists: [const { Vec::new() }; CLASSES],
                record: None,
            })
        };
    }

    /// A free block of `class`, if this thread keeps one or was given one
    /// back, and the owner of the block allocated for it either way.
    pub(super) fn take(class: usize) -> (Option<NonNull<u8>>, Owner) {
        CACHE
            .try_with(|cache| {
                let Ok(mut cache) = cache.try_borrow_mut() else {
                    return (None, Owner::NONE);
                };
                let record = *cache.record.get_or_insert_with(claim);
                if cache.lists[class].is_empty() {
                    cache.take_back(record);
                }
                (cache.lists[class].pop(), Owner(record))
            })
            .unwrap_or((None, Owner::NONE))
    }

    /// Keeps `ptr`, a block of `class` from `owner`: back to its owner if
    /// that is another thread, else (or if the owner's list refuses it) in
    /// this thread's cache; `false` if neither takes it.
    pub(super) fn keep(class: usize, ptr: NonNull<u8>, owner: Owner) -> bool {
        // SAFETY: records are never freed.
        let owner = unsafe { owner.0.as_ref() };
        CACHE
            .try_with(|cache| {
                let mut cache = cache.try_borrow_mut().ok();
                let mine = cache
                    .as_ref()
                    .and_then(|cache| cache.record)
                    .is_some_and(|record| owner.is_some_and(|o| ptr::eq(o, record)));
                if !mine
                    && let Some(owner) = owner
                    && give_back(owner, ptr, class)
                {
                    return true;
                }
                cache.as_mut().is_some_and(|cache| cache.keep(class, ptr))
            })
            // This thread's cache is gone (it is ending).
            .unwrap_or_else(|_| owner.is_some_and(|o| give_back(o, ptr, class)))
    }
}

#[cfg(all(loom, test))]
mod cache {
    use core::ptr::NonNull;

    use super::Owner;

    pub(super) struct Record;

    pub(super) fn take(_class: usize) -> (Option<NonNull<u8>>, Owner) {
        (None, Owner::NONE)
    }

    pub(super) fn keep(_class: usize, _ptr: NonNull<u8>, _owner: Owner) -> bool {
        false
    }
}

/// Memory for a task of `layout`, and the owner to free it with.
pub(crate) fn alloc(layout: Layout) -> (NonNull<u8>, Owner) {
    let (layout, owner) = match class_of(layout) {
        Some(class) => match cache::take(class) {
            (Some(ptr), owner) => return (ptr, owner),
            (None, owner) => (block(class), owner),
        },
        None => (layout, Owner::NONE),
    };
    // SAFETY: the layout is nonzero in size (a task holds its header).
    let ptr = unsafe { std::alloc::alloc(layout) };
    let ptr = NonNull::new(ptr).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
    (ptr, owner)
}

/// Frees the memory of a task of `layout` at `ptr`, which [`alloc`] gave
/// with `owner`.
///
/// # Safety
///
/// `ptr` and `owner` came from `alloc(layout)`, and nothing uses `ptr` any
/// more.
pub(crate) unsafe fn free(ptr: NonNull<u8>, layout: Layout, owner: Owner) {
    match class_of(layout) {
        Some(class) => {
            if !cache::keep(class, ptr, owner) {
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

    /// A block of the class of 200 bytes, as a task's would be.
    const LAYOUT: Layout = match Layout::from_size_align(200, 16) {
        Ok(layout) => layout,
        Err(_) => panic!(),
    };

    /// A block's address and owner, to hand to another thread.
    struct Sent(NonNull<u8>, Owner);
    // SAFETY: the block is no task's; it is only freed on the other side.
    unsafe impl Send for Sent {}

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
        let (a, owner) = alloc(LAYOUT);
        // SAFETY: just allocated, unused.
        unsafe { free(a, LAYOUT, owner) };
        let other = Layout::from_size_align(250, 8).unwrap();
        let (b, owner) = alloc(other);
        assert_eq!(a, b, "the same class, the same block");
        // SAFETY: as above.
        unsafe { free(b, other, owner) };
        let big = Layout::from_size_align(STEP * CLASSES * 2, 8).unwrap();
        let (c, owner) = alloc(big);
        // SAFETY: as above.
        unsafe { free(c, big, owner) };
    }

    /// On a thread of its own, so its cache starts empty: a block freed on
    /// another thread is the owner's next block of its class, not a new one.
    #[test]
    fn a_block_freed_on_another_thread_goes_back_to_its_owner() {
        std::thread::spawn(|| {
            let (a, owner) = alloc(LAYOUT);
            let sent = Sent(a, owner);
            std::thread::spawn(move || {
                let sent = sent;
                // SAFETY: allocated with `LAYOUT`, unused.
                unsafe { free(sent.0, LAYOUT, sent.1) };
            })
            .join()
            .unwrap();
            let (b, owner) = alloc(LAYOUT);
            assert_eq!(a, b, "the block came back");
            // SAFETY: as above.
            unsafe { free(b, LAYOUT, owner) };
        })
        .join()
        .unwrap();
    }

    /// A block its owner's list refuses (closed: its thread ended) stays
    /// with the thread that freed it, as that thread's next block of the
    /// class.
    #[test]
    fn a_block_its_owner_refuses_stays_with_the_thread_that_freed_it() {
        std::thread::spawn(|| {
            let (a, _) = alloc(LAYOUT);
            let closed = Owner(&cache::CLOSED_RECORD);
            // SAFETY: allocated with `LAYOUT`, unused.
            unsafe { free(a, LAYOUT, closed) };
            let (b, owner) = alloc(LAYOUT);
            assert_eq!(a, b, "kept here");
            // SAFETY: as above.
            unsafe { free(b, LAYOUT, owner) };
        })
        .join()
        .unwrap();
    }

    /// The owner's list takes `KEPT_BYTES` of blocks; the next stays with
    /// the thread that freed it, and the owner gets the rest back.
    #[test]
    fn an_owners_list_takes_at_most_its_bound() {
        std::thread::spawn(|| {
            let bound = KEPT_BYTES / block(class_of(LAYOUT).unwrap()).size();
            let blocks: Vec<_> = (0..=bound)
                .map(|_| {
                    let (ptr, owner) = alloc(LAYOUT);
                    Sent(ptr, owner)
                })
                .collect();
            let addrs: Vec<_> = blocks.iter().map(|b| b.0).collect();
            let last = std::thread::spawn(move || {
                let mut last = None;
                for sent in blocks {
                    // SAFETY: allocated with `LAYOUT`, unused.
                    unsafe { free(sent.0, LAYOUT, sent.1) };
                    last = Some(sent.0);
                }
                // The one past the bound is this thread's now.
                let (b, owner) = alloc(LAYOUT);
                assert_eq!(Some(b), last);
                // SAFETY: as above.
                unsafe { free(b, LAYOUT, owner) };
            });
            last.join().unwrap();
            let back: Vec<_> = (0..bound).map(|_| alloc(LAYOUT)).collect();
            for (b, _) in &back {
                assert!(addrs[..bound].contains(b), "a block given back");
            }
            for (b, owner) in back {
                // SAFETY: as above.
                unsafe { free(b, LAYOUT, owner) };
            }
        })
        .join()
        .unwrap();
    }
}
