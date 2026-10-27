//! The message queue behind a channel: a single-consumer queue of segments,
//! on kovan for their reclamation, kept behind one small type so a
//! model-checking build can swap it.
//!
//! Any number of senders push and one receiver takes (from its receive calls
//! and from its drop), so the queue is built for exactly that, and the
//! receiver's end is its own: a pop reads the next cell and nothing else
//! another thread writes, with no read-modify-write anywhere on it. The
//! protocol the channel relies on is small:
//!
//! - Values come out in the order their pushes claimed positions. A push that
//!   returned before another began claimed the earlier position, so one
//!   sender's values come out in the order it sent them.
//! - A `push` that has returned is visible to every `try_pop` that starts
//!   after it.
//! - A value is taken at most once, and a value still queued when the queue
//!   is dropped is dropped exactly once.
//! - [`try_pop`](Queue::try_pop) never waits. When the next position is
//!   claimed by a push that has not written its value yet, it answers `None`,
//!   as for an empty queue, and takes nothing: the receiver registers its
//!   waker and returns `Pending`, and the pusher wakes it after its push
//!   returns, as it does for every push. (A sender that is still writing
//!   still counts as a sender, so the receiver never reads that as the end.)
//!
//! # Segments
//!
//! Positions are claimed with one `fetch_add` on the tail's word; position
//! `p` is cell `p % SEGMENT` of the segment that starts at `p - p % SEGMENT`.
//! A push walks to that segment from the tail's segment pointer, loaded
//! before its claim and so at or before it, linking a fresh segment where
//! the list ends, writes the value and marks the cell ready with a release
//! store. The receiver reads its cell's ready mark, takes the value and
//! moves its own position on; a used-up segment is left once its successor
//! is linked.
//!
//! # Memory reclamation
//!
//! The receiver retires the segment it leaves through kovan, after it
//! published the start of its new head segment and moved the tail's segment
//! pointer off the one it leaves. No pointer anyone loads from then on names
//! that segment, but a push may still read it from a segment it walks
//! through: kovan protects a node only if it was loaded while reachable
//! (`kovan::Atomic::load`), so a push checks the head's start before it steps
//! onto a successor, computing the successor's start from the segment it is
//! on rather than reading it, and starts its walk again from the head's
//! published segment when the receiver passed it. A retired segment holds no
//! value (the receiver took every one, and every push into it returned), so
//! it is freed through a type-erased header that knows only its layout, and
//! the queue needs no `T: 'static`.
//!
//! The ready mark's store and the receiver's loads of a mark and of a link are
//! sequentially consistent: they are this queue's half of the channel's wake
//! handshake, in which a send wakes the receiver only if it says it waits
//! (see `chan`).
//!
//! # Under loom
//!
//! kovan is not instrumented, so loom cannot interleave its steps. Under
//! `--cfg loom` the same type is a loom `Mutex` around a deque of cells, and a
//! push is two steps, as here: it claims the next cell, then writes its value
//! into it. So a receiver can find the next cell claimed and empty, and the
//! models check the handshake the channel builds around that answer (the
//! waker, the counts, the close bit), and count how often their search met
//! it. The same split is made for the cancellation token's registry (see
//! `cancellation/rc.rs`). proofs/tla/segments checks this queue's own steps.

#![allow(unsafe_code)]

/// The queue of one channel.
#[cfg(not(loom))]
pub(super) use self::segments::Queue;

#[cfg(not(loom))]
mod segments {
    use core::cell::UnsafeCell;
    use core::marker::PhantomData;
    use core::mem::MaybeUninit;
    use core::ptr;

    use kovan::{
        Atomic, CachePadded, Reclaimable, RetiredNode, Shared as Ptr, alloc_node_uninit, free_node,
        pin, retire_reclaimable,
    };

    use crate::sync::internal::{AtomicBool, AtomicU64, Ordering};

    /// Cells per segment.
    const SEGMENT: u64 = 32;

    struct Cell<T> {
        /// The value is written: release-stored once by its push.
        ready: AtomicBool,
        value: UnsafeCell<MaybeUninit<T>>,
    }

    /// The type-erased head of every segment, the node kovan retires.
    ///
    /// kovan's [`retire_reclaimable`] needs a `'static` node type; `Header` is
    /// one whatever `T` is, holding kovan's [`RetiredNode`] and a function,
    /// made for the segment's `T`, that frees the segment's memory. A segment
    /// is retired only once the receiver took every value in it and every
    /// push into it returned, and kovan calls `dealloc` only after every guard
    /// that could reach it is gone; `free_segment` then frees the layout of
    /// `Segment<T>`, which depends on the size and alignment of `T` alone, and
    /// touches no `T`. So freeing it after the data some `T` borrows is gone is
    /// sound, as in kovan-queue's segment queue.
    #[repr(C)]
    struct Header {
        retired: RetiredNode,
        free: unsafe fn(*mut Header),
    }

    // SAFETY: `RetiredNode` is the first field of the `#[repr(C)]` `Header`,
    // and `Header` the first of every `#[repr(C)]` `Segment<T>`, so the
    // segment's address is the node's. `dealloc` only frees memory (see
    // `Header`) and cannot panic.
    unsafe impl Reclaimable for Header {
        fn retired_node(&self) -> &RetiredNode {
            &self.retired
        }

        fn retired_node_mut(&mut self) -> &mut RetiredNode {
            &mut self.retired
        }

        unsafe fn dealloc(node: *mut Self) {
            // SAFETY: kovan calls this once, when no thread can reach the
            // segment; `free` was set for its `T` when it was made.
            unsafe { ((*node).free)(node) }
        }
    }

    /// Frees a segment of `T`s without touching any `T`.
    ///
    /// # Safety
    ///
    /// `header` heads a `Segment<T>` from [`Segment::alloc`] that no thread
    /// can reach any more and whose cells hold no value.
    unsafe fn free_segment<T>(header: *mut Header) {
        // SAFETY: the caller's contract; a segment has no drop glue of its own
        // (its values sit in `MaybeUninit`s) and `free_node` drops nothing.
        unsafe { free_node(header.cast::<Segment<T>>()) }
    }

    #[repr(C)]
    struct Segment<T> {
        header: Header,
        /// The successor, linked once by the push that found the list ending
        /// here.
        next: Atomic<Segment<T>>,
        /// The position of its first cell.
        start: u64,
        cells: [Cell<T>; SEGMENT as usize],
    }

    impl<T> Segment<T> {
        /// A segment whose cells are all empty, starting at `start`.
        fn alloc(start: u64) -> *mut Self {
            let p = alloc_node_uninit::<Self>().cast::<Self>();
            // SAFETY: a fresh allocation of a segment. Every field but the
            // values is written; a value is a `MaybeUninit`, valid
            // uninitialised, so the whole segment is initialised afterwards.
            unsafe {
                (&raw mut (*p).header).write(Header {
                    retired: RetiredNode::new(),
                    free: free_segment::<T>,
                });
                (&raw mut (*p).next).write(Atomic::null());
                (&raw mut (*p).start).write(start);
                let cells = (&raw mut (*p).cells).cast::<Cell<T>>();
                for i in 0..SEGMENT as usize {
                    (&raw mut (*cells.add(i)).ready).write(AtomicBool::new(false));
                }
            }
            p
        }
    }

    /// The pushes' end.
    struct Tail<T> {
        /// The next position a push claims.
        pos: AtomicU64,
        /// Where a push starts its walk: at or before the segment of every
        /// position not claimed yet, and never a retired segment.
        seg: Atomic<Segment<T>>,
    }

    /// The receiver's end.
    struct Head<T> {
        /// The segment of the next position to take; the receiver's alone.
        seg: UnsafeCell<*mut Segment<T>>,
        /// The next position to take: written by the receiver only, read by
        /// [`Queue::len`].
        pos: AtomicU64,
        /// The head segment, for a push whose walk the receiver passed.
        published: Atomic<Segment<T>>,
        /// The head segment's start: a segment that starts before it may be
        /// retired, so a push steps onto no such segment.
        start: AtomicU64,
    }

    /// The queue of one channel.
    pub(in crate::sync::mpsc) struct Queue<T> {
        tail: CachePadded<Tail<T>>,
        head: CachePadded<Head<T>>,
        /// The queue owns the values in its cells.
        _values: PhantomData<T>,
    }

    // SAFETY: values move between threads through the queue, which is all
    // `T: Send` allows; the receiver's end is reached by one thread at a time
    // (`try_pop`'s contract), and every shared word is an atomic.
    unsafe impl<T: Send> Send for Queue<T> {}
    // SAFETY: as above.
    unsafe impl<T: Send> Sync for Queue<T> {}

    impl<T> Queue<T> {
        pub(in crate::sync::mpsc) fn new() -> Self {
            let first = Segment::<T>::alloc(0);
            Self {
                tail: CachePadded::new(Tail {
                    pos: AtomicU64::new(0),
                    seg: Atomic::new(first),
                }),
                head: CachePadded::new(Head {
                    seg: UnsafeCell::new(first),
                    pos: AtomicU64::new(0),
                    published: Atomic::new(first),
                    start: AtomicU64::new(0),
                }),
                _values: PhantomData,
            }
        }

        /// Appends `value`. Lock-free: its walk starts again only when the
        /// receiver passed the segment it was about to step onto, at most once
        /// per segment the receiver leaves meanwhile.
        pub(in crate::sync::mpsc) fn push(&self, value: T) {
            let guard = pin();
            // Loaded before the claim: at or before the claimed position's
            // segment.
            let first = self.tail.seg.load(Ordering::SeqCst, &guard);
            let pos = self.tail.pos.fetch_add(1, Ordering::SeqCst);
            let target = pos - pos % SEGMENT;
            let mut seg = first;
            // A segment this push made and could not link, for the next one
            // it links; freed at the end if none.
            let mut spare: *mut Segment<T> = ptr::null_mut();
            let (at_ptr, at) = loop {
                // SAFETY: `seg` was loaded while reachable: the tail's or the
                // head's pointer (never a retired segment), or a successor
                // stepped onto only after the head's start showed the receiver
                // had not passed it; the guard keeps it.
                let s = unsafe { seg.deref() };
                if s.start == target {
                    break (seg, s);
                }
                let next_start = s.start + SEGMENT;
                if s.next.load(Ordering::SeqCst, &guard).is_null() {
                    if spare.is_null() {
                        spare = Segment::alloc(next_start);
                    } else {
                        // SAFETY: unpublished: this push is its only user.
                        unsafe { (*spare).start = next_start };
                    }
                    // SAFETY: a fresh segment, linked once if the swap wins;
                    // and the null pointer the swap expects.
                    let (fresh, none) =
                        unsafe { (Ptr::from_raw(spare), Ptr::from_raw(ptr::null_mut())) };
                    if s.next
                        .compare_exchange(none, fresh, Ordering::SeqCst, Ordering::SeqCst, &guard)
                        .is_ok()
                    {
                        spare = ptr::null_mut();
                    }
                }
                // Loaded (not taken from the swap) so the guard protects it,
                // then checked: a successor of a segment the receiver passed
                // may be retired, and freed, already.
                let next = s.next.load(Ordering::SeqCst, &guard);
                seg = if self.head.start.load(Ordering::SeqCst) > next_start {
                    self.head.published.load(Ordering::SeqCst, &guard)
                } else {
                    next
                };
            };
            // The tail's pointer moves on to this segment, if it still names
            // where this walk began. Before the value is ready: the receiver
            // can leave the segment only after it, and moves the pointer off
            // it first.
            // `at` holds this push's cell, not written yet, so the receiver has
            // not passed it: reachable, not retired.
            if at_ptr != first {
                let _ = self.tail.seg.compare_exchange(
                    first,
                    at_ptr,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                    &guard,
                );
            }
            let cell = &at.cells[(pos - target) as usize];
            // SAFETY: the position is this push's alone, and the receiver reads
            // the value only after it reads the ready mark stored below.
            unsafe { cell.value.get().write(MaybeUninit::new(value)) };
            // SeqCst: the first half of the channel's wake handshake, with the
            // receiver's look after it says it waits (see `chan`).
            cell.ready.store(true, Ordering::SeqCst);
            if !spare.is_null() {
                // SAFETY: never published, and holds no value.
                unsafe { free_node(spare) };
            }
        }

        /// Takes the oldest value, without waiting: `None` when there is none
        /// yet, or when the next one is still being written.
        ///
        /// # Safety
        ///
        /// The caller is the queue's one receiver: no two calls of this
        /// function on one queue run at once.
        pub(in crate::sync::mpsc) unsafe fn try_pop(&self) -> Option<T> {
            let head = &*self.head;
            let pos = head.pos.load(Ordering::Relaxed);
            // SAFETY: the caller's contract: the head's segment is ours, and
            // only this end retires a segment, never the one it is on.
            let mut seg = unsafe { &**head.seg.get() };
            if pos - seg.start == SEGMENT {
                // Used up: on to the successor, once a push linked it.
                let guard = pin();
                // SAFETY: segments from the head on are retired by this end
                // alone, so the successor stays allocated without the guard's
                // protection.
                // SeqCst: in the wake handshake, as the cell's mark below.
                let next_ptr =
                    unsafe { seg.next.load_unprotected(Ordering::SeqCst, &guard) }.as_raw();
                if next_ptr.is_null() {
                    return None;
                }
                // SAFETY: the caller's contract: the head's pointer is ours.
                let old_ptr = unsafe { *head.seg.get() };
                // SAFETY: as above, both stay ours; `next` is linked, so made.
                unsafe { self.leave(old_ptr, next_ptr, &guard) };
                // SAFETY: the caller's contract.
                unsafe { *head.seg.get() = next_ptr };
                // SAFETY: as above.
                seg = unsafe { &*next_ptr };
            }
            let cell = &seg.cells[(pos - seg.start) as usize];
            // SeqCst: the receiver's look in the channel's wake handshake: after
            // it says it waits, this load or the push's look at that sees the
            // other (see `chan`).
            if !cell.ready.load(Ordering::SeqCst) {
                return None;
            }
            // SAFETY: ready: its push wrote the value before the store this load
            // read; the position is passed below, so it is moved out once.
            let value = unsafe { cell.value.get().read().assume_init() };
            head.pos.store(pos + 1, Ordering::Relaxed);
            Some(value)
        }

        /// The receiver leaves `old` for its successor `next`: it says so to
        /// the pushes, moves the tail's pointer off `old`, and retires it.
        ///
        /// # Safety
        ///
        /// The caller is the receiver; `old` is its head segment, used up, and
        /// `next` its linked successor.
        unsafe fn leave(
            &self,
            old_ptr: *mut Segment<T>,
            next_ptr: *mut Segment<T>,
            guard: &kovan::Guard,
        ) {
            // SAFETY: the caller's contract: both are live segments of this
            // queue, and `next` is fully made.
            let (old_s, next_s, next_start) = unsafe {
                (
                    Ptr::from_raw(old_ptr),
                    Ptr::from_raw(next_ptr),
                    (*next_ptr).start,
                )
            };
            // The segment first, then its start: a push that sees the new
            // start and starts again finds the new segment.
            self.head.published.store(next_s, Ordering::SeqCst);
            self.head.start.store(next_start, Ordering::SeqCst);
            // The tail's pointer is at or past `old` (it never names a segment
            // this end left); if on it, it moves to `next`, so no walk begins
            // there from now on. A swap that fails found it moved on already.
            let _ = self.tail.seg.compare_exchange(
                old_s,
                next_s,
                Ordering::SeqCst,
                Ordering::SeqCst,
                guard,
            );
            // SAFETY: every value of `old` was taken and every push into it
            // returned; no pointer loaded from now on names it, and a walk
            // that read it from a segment before checks the start published
            // above before it steps on. Retired once, here.
            unsafe { retire_reclaimable(old_ptr.cast::<Header>()) };
        }

        /// How many values are queued, as a snapshot: a push in progress
        /// counts, a pop in progress does not.
        pub(in crate::sync::mpsc) fn len(&self) -> usize {
            // The head first: the tail read after it is at or past it.
            let front = self.head.pos.load(Ordering::Acquire);
            let back = self.tail.pos.load(Ordering::Acquire);
            usize::try_from(back.saturating_sub(front)).unwrap_or(usize::MAX)
        }
    }

    impl<T> Drop for Queue<T> {
        fn drop(&mut self) {
            // Nothing else reaches the queue: every push returned, so every
            // position claimed holds its value.
            let end = *self.tail.pos.get_mut();
            let mut pos = *self.head.pos.get_mut();
            let mut seg = *self.head.seg.get_mut();
            while !seg.is_null() {
                // SAFETY: the head's segment and those linked after it are this
                // queue's, retired by nobody; the queue is gone after this.
                let s = unsafe { &mut *seg };
                while pos < end && pos < s.start + SEGMENT {
                    let cell = &mut s.cells[(pos - s.start) as usize];
                    if *cell.ready.get_mut() {
                        // SAFETY: written and not taken: dropped once, here.
                        unsafe { cell.value.get_mut().assume_init_drop() };
                    }
                    pos += 1;
                }
                let guard = pin();
                // SAFETY: as above, nothing else reaches the list.
                let next = unsafe { s.next.load_unprotected(Ordering::Relaxed, &guard) }.as_raw();
                drop(guard);
                // SAFETY: no value is left in it and nothing reaches it.
                unsafe { free_node(seg) };
                seg = next;
            }
        }
    }

    #[cfg(test)]
    pub(in crate::sync::mpsc) const SEGMENT_CELLS: u64 = SEGMENT;
}

/// The queue of one channel, modelled for loom.
#[cfg(loom)]
pub(super) struct Queue<T>(loom::sync::Mutex<Cells<T>>);

/// The loom model's cells: the claimed ones from the oldest on, `None` while
/// the push that claimed it has not written it yet.
#[cfg(loom)]
pub(super) struct Cells<T> {
    /// How many cells were taken: the position of the deque's front.
    taken: u64,
    cells: std::collections::VecDeque<Option<T>>,
}

#[cfg(loom)]
impl<T> Queue<T> {
    pub(super) fn new() -> Self {
        Self(loom::sync::Mutex::new(Cells {
            taken: 0,
            cells: std::collections::VecDeque::new(),
        }))
    }

    /// Claims the next cell, then writes it, in two steps.
    pub(super) fn push(&self, value: T) {
        let at = {
            let mut q = self.lock();
            q.cells.push_back(None);
            q.taken + q.cells.len() as u64 - 1
        };
        let mut q = self.lock();
        // Only a written cell is taken, so the claimed one is still here.
        let index = (at - q.taken) as usize;
        q.cells[index] = Some(value);
    }

    /// As the real queue's: `None` for an empty queue and for a next cell
    /// claimed and not written yet (counted for the models).
    ///
    /// # Safety
    ///
    /// None needed here; the signature is the real queue's.
    pub(super) unsafe fn try_pop(&self) -> Option<T> {
        let mut q = self.lock();
        match q.cells.front_mut() {
            None => None,
            Some(cell) => match cell.take() {
                Some(value) => {
                    q.cells.pop_front();
                    q.taken += 1;
                    Some(value)
                }
                None => {
                    crate::sync::loom_model::busy_look();
                    None
                }
            },
        }
    }

    pub(super) fn len(&self) -> usize {
        self.lock().cells.len()
    }

    fn lock(&self) -> loom::sync::MutexGuard<'_, Cells<T>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::segments::SEGMENT_CELLS;
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The test is the queue's one receiver.
    fn pop<T>(queue: &Queue<T>) -> Option<T> {
        // SAFETY: each test pops from one thread only.
        unsafe { queue.try_pop() }
    }

    #[test]
    fn values_of_one_pusher_come_out_in_order() {
        let queue = Queue::new();
        for i in 0..100 {
            queue.push(i);
        }
        assert_eq!(queue.len(), 100);
        for i in 0..100 {
            assert_eq!(pop(&queue), Some(i));
        }
        assert_eq!(pop(&queue), None);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn a_queue_drained_at_each_segment_end_goes_on_into_the_next() {
        let queue = Queue::new();
        let n = SEGMENT_CELLS * 3;
        for round in 0..4 {
            for i in 0..n {
                queue.push(round * n + i);
                assert_eq!(pop(&queue), Some(round * n + i));
                assert_eq!(pop(&queue), None, "taken once");
            }
        }
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn a_value_left_in_the_queue_is_dropped_once_with_it() {
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let queue = Queue::new();
        // Across segments, and starting past the first: the drop walks from
        // the head's segment.
        for _ in 0..SEGMENT_CELLS + 6 {
            queue.push(Counted(Arc::clone(&drops)));
        }
        for _ in 0..SEGMENT_CELLS + 2 {
            assert!(pop(&queue).is_some());
        }
        assert_eq!(drops.load(Ordering::SeqCst), (SEGMENT_CELLS + 2) as usize);
        drop(queue);
        assert_eq!(drops.load(Ordering::SeqCst), (SEGMENT_CELLS + 6) as usize);
    }

    /// Values that borrow from the test's frame: the queue needs no
    /// `T: 'static`, and frees its segments without touching a value.
    #[test]
    fn values_that_borrow_go_through() {
        let words = [String::from("a"), String::from("b")];
        let queue = Queue::new();
        for _ in 0..SEGMENT_CELLS * 2 + 1 {
            queue.push(&words[0]);
            queue.push(&words[1]);
        }
        let mut got = 0;
        while let Some(w) = pop(&queue) {
            assert!(w == "a" || w == "b");
            got += 1;
        }
        assert_eq!(got, SEGMENT_CELLS * 4 + 2);
        drop(queue);
        kovan::flush();
    }

    /// Pushes from several threads while the receiver takes: every value
    /// once, each pusher's in its order, across many segments.
    /// Under Miri, few enough to finish and still cross segments, so its race
    /// detector sees the walks, the links and the retirements.
    #[test]
    fn concurrent_pushes_each_arrive_once_and_in_order() {
        const PUSHERS: u64 = 4;
        const EACH: u64 = if cfg!(miri) {
            3 * SEGMENT_CELLS
        } else {
            20_000
        };
        let queue = Arc::new(Queue::<(u64, u64)>::new());
        let pushers: Vec<_> = (0..PUSHERS)
            .map(|p| {
                let queue = Arc::clone(&queue);
                std::thread::spawn(move || {
                    for i in 0..EACH {
                        queue.push((p, i));
                    }
                })
            })
            .collect();
        let mut next = [0u64; PUSHERS as usize];
        let mut got = 0;
        while got < PUSHERS * EACH {
            match pop(&queue) {
                Some((p, i)) => {
                    assert_eq!(i, next[p as usize], "pusher {p} out of order");
                    next[p as usize] += 1;
                    got += 1;
                }
                None => std::hint::spin_loop(),
            }
        }
        for p in pushers {
            p.join().unwrap();
        }
        assert_eq!(pop(&queue), None);
        assert_eq!(queue.len(), 0);
    }
}
