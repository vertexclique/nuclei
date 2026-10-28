//! The bounded channel's queue, for a capacity the ring holds: a ring of
//! cells, made a segment at a time on first use and used lap after lap, with
//! nothing to reclaim while the channel lives.
//!
//! The ring has `next_power_of_two(capacity)` cells in segments of up to
//! `MAX_SEGMENT` cells, so position `p` is cell `p % cells` of segment
//! `(p / cells) % segments`, two masks. A cell is ready for position `p` when
//! its stamp is `p + 1`, so a stamp of an earlier lap is never taken for a
//! value of this one.
//!
//! A send claims its position with one fetch-add only after the channel's
//! semaphore admitted it, and the receiver gives the admission back only
//! after it took its value. Every position claimed and not taken holds an
//! admission, so fewer than `capacity` of them, at most the ring's cells,
//! lie between the receiver's position and a claim: a send never reaches a
//! cell whose earlier value is not taken, and the receiver's read of it
//! happened before the admission it gave back. A send loads its segment
//! (making it with one compare-and-swap the first time, a swap it loses
//! leaving the other send's), writes its value and stamps the cell: wait-free,
//! a bounded number of its own steps, and no allocation once every segment
//! was made. The receiver reads its cell's stamp and nothing else another
//! thread writes.
//!
//! The stamp's store and the receiver's loads are sequentially consistent:
//! this queue's half of the channel's wake handshake (see `chan`).
//! proofs/tla/chanring checks these steps.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr;

use kovan::CachePadded;

use crate::sync::internal::{AtomicPtr, AtomicU64, Ordering};

/// The most cells a segment holds.
const MAX_SEGMENT: u64 = 32;
/// The most cells a ring holds: past it, a bounded channel's queue is a
/// radix directory instead (the ring's segment table is made whole at once).
pub(super) const MAX_CELLS: u64 = 1 << 16;

struct Cell<T> {
    /// One past the position whose value it holds; 0 before its first.
    stamp: AtomicU64,
    value: UnsafeCell<MaybeUninit<T>>,
}

/// Entries of the segment table per chunk: a chunk is its own cache lines,
/// so loads of the table, read-only once its segments are made, never share a
/// line with a word another thread writes.
const CHUNK: usize = 16;

#[repr(align(128))]
struct Chunk<T>([AtomicPtr<Cell<T>>; CHUNK]);

/// The receiver's end.
struct Head<T> {
    /// The next position to take: written by the receiver only, read by
    /// [`Ring::len`].
    pos: AtomicU64,
    /// The segment of the last take and its number: the receiver's alone, so
    /// a take within the same segment loads nothing but its cell.
    last: UnsafeCell<(u64, *mut Cell<T>)>,
}

/// The queue.
pub(in crate::sync::mpsc) struct Ring<T> {
    /// The next position a send claims.
    tail: CachePadded<AtomicU64>,
    head: CachePadded<Head<T>>,
    /// Each segment, in chunks: null until a send makes it, then `cells`
    /// cells.
    segments: Box<[Chunk<T>]>,
    /// log2 of the cells a segment holds.
    cell_bits: u32,
    /// The cells a segment holds, less one.
    cell_mask: u64,
    /// The segments, less one.
    segment_mask: u64,
    /// The queue owns the values in its cells.
    _values: PhantomData<T>,
}

// SAFETY: values move between threads through the queue, which is all
// `T: Send` allows; the receiver's end is reached by one thread at a time
// (`try_pop`'s contract), and every shared word is an atomic.
unsafe impl<T: Send> Send for Ring<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for Ring<T> {}

impl<T> Ring<T> {
    /// A ring for a channel of `capacity`, at most [`MAX_CELLS`].
    pub(in crate::sync::mpsc) fn new(capacity: usize) -> Self {
        let cells = (capacity as u64).max(1).next_power_of_two();
        debug_assert!(cells <= MAX_CELLS, "a ring holds at most MAX_CELLS cells");
        let per_segment = cells.min(MAX_SEGMENT);
        let segments = (cells / per_segment) as usize;
        Self {
            tail: CachePadded::new(AtomicU64::new(0)),
            head: CachePadded::new(Head {
                pos: AtomicU64::new(0),
                last: UnsafeCell::new((u64::MAX, ptr::null_mut())),
            }),
            segments: (0..segments.div_ceil(CHUNK))
                .map(|_| Chunk(core::array::from_fn(|_| AtomicPtr::new(ptr::null_mut()))))
                .collect(),
            cell_bits: per_segment.trailing_zeros(),
            cell_mask: per_segment - 1,
            segment_mask: segments as u64 - 1,
            _values: PhantomData,
        }
    }

    fn per_segment(&self) -> usize {
        1 << self.cell_bits
    }

    /// The table's entry of segment `number`.
    fn slot(&self, number: u64) -> &AtomicPtr<Cell<T>> {
        let number = number as usize;
        &self.segments[number / CHUNK].0[number % CHUNK]
    }

    /// The cell of position `pos`, its segment made if `make` and missing;
    /// null if missing and not.
    fn cell(&self, pos: u64, make: bool) -> *const Cell<T> {
        let slot = self.slot((pos >> self.cell_bits) & self.segment_mask);
        // SeqCst: in the wake handshake, as the stamp's load.
        let mut segment = slot.load(Ordering::SeqCst);
        if segment.is_null() {
            if !make {
                return ptr::null();
            }
            segment = self.make_segment(slot);
        }
        // SAFETY: a segment of `per_segment` cells, kept for the queue's life.
        unsafe { segment.add((pos & self.cell_mask) as usize) }
    }

    /// The segment of `slot`, made now: once per segment, so out of line.
    #[cold]
    #[inline(never)]
    fn make_segment(&self, slot: &AtomicPtr<Cell<T>>) -> *mut Cell<T> {
        let fresh = Self::alloc_segment(self.per_segment());
        match slot.compare_exchange(ptr::null_mut(), fresh, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => fresh,
            Err(made) => {
                // SAFETY: never published.
                unsafe { Self::free_segment(fresh, self.per_segment()) };
                made
            }
        }
    }

    fn alloc_segment(cells: usize) -> *mut Cell<T> {
        let segment: Box<[Cell<T>]> = (0..cells)
            .map(|_| Cell {
                stamp: AtomicU64::new(0),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            })
            .collect();
        Box::into_raw(segment).cast::<Cell<T>>()
    }

    /// # Safety
    ///
    /// `segment` came from [`alloc_segment`](Self::alloc_segment) with
    /// `cells`, nothing else reaches it, and its cells hold no value.
    unsafe fn free_segment(segment: *mut Cell<T>, cells: usize) {
        // SAFETY: the caller's contract; the values sit in `MaybeUninit`s.
        drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(segment, cells)) });
    }

    /// Appends `value`, which the channel's semaphore admitted. Wait-free:
    /// see the module.
    #[inline]
    pub(in crate::sync::mpsc) fn push(&self, value: T) {
        let pos = self.tail.fetch_add(1, Ordering::AcqRel);
        // SAFETY: made, and kept for the queue's life.
        let cell = unsafe { &*self.cell(pos, true) };
        // SAFETY: the position is this send's alone, and its cell's earlier
        // value was taken before the admission this send holds was given back
        // (see the module); the receiver reads it only after the stamp below.
        unsafe { cell.value.get().write(MaybeUninit::new(value)) };
        // SeqCst: the first half of the channel's wake handshake.
        cell.stamp.store(pos + 1, Ordering::SeqCst);
    }

    /// Takes the oldest value, without waiting: `None` when there is none
    /// yet, or when the next one is still being written.
    ///
    /// # Safety
    ///
    /// The caller is the queue's one receiver: no two calls of this function
    /// on one queue run at once.
    #[inline]
    pub(in crate::sync::mpsc) unsafe fn try_pop(&self) -> Option<T> {
        let pos = self.head.pos.load(Ordering::Relaxed);
        let number = (pos >> self.cell_bits) & self.segment_mask;
        // SAFETY: the caller's contract: the cache is the receiver's.
        let last = unsafe { &mut *self.head.last.get() };
        if last.0 != number {
            // SeqCst: a segment not made yet is part of the receiver's look in
            // the wake handshake, as the stamp's load below.
            let segment = self.slot(number).load(Ordering::SeqCst);
            if segment.is_null() {
                return None;
            }
            *last = (number, segment);
        }
        // SAFETY: a segment, once made, is kept for the queue's life.
        let cell = unsafe { &*last.1.add((pos & self.cell_mask) as usize) };
        // SeqCst: the receiver's look in the channel's wake handshake.
        if cell.stamp.load(Ordering::SeqCst) != pos + 1 {
            return None;
        }
        // SAFETY: stamped for this position: its send wrote the value first;
        // the position is passed below, so it is moved out once.
        let value = unsafe { cell.value.get().read().assume_init() };
        self.head.pos.store(pos + 1, Ordering::Relaxed);
        Some(value)
    }

    /// How many values are queued, as a snapshot: a push in progress counts,
    /// a pop in progress does not.
    pub(in crate::sync::mpsc) fn len(&self) -> usize {
        let front = self.head.pos.load(Ordering::Acquire);
        let back = self.tail.load(Ordering::Acquire);
        usize::try_from(back.saturating_sub(front)).unwrap_or(usize::MAX)
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        // Nothing else reaches the queue: every push returned, so every
        // position claimed and not taken holds its value.
        let head = *self.head.pos.get_mut();
        let tail = *self.tail.get_mut();
        for pos in head..tail {
            let cell = self.cell(pos, false).cast_mut();
            if cell.is_null() {
                continue;
            }
            // SAFETY: exclusive access.
            let cell = unsafe { &mut *cell };
            if *cell.stamp.get_mut() == pos + 1 {
                // SAFETY: written and not taken: dropped once, here.
                unsafe { cell.value.get_mut().assume_init_drop() };
            }
        }
        let cells = self.per_segment();
        for slot in self
            .segments
            .iter_mut()
            .flat_map(|chunk| chunk.0.iter_mut())
        {
            let segment = *slot.get_mut();
            if !segment.is_null() {
                // SAFETY: made by this queue, nothing else reaches it, and its
                // values were dropped above or taken.
                unsafe { Self::free_segment(segment, cells) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    fn pop<T>(ring: &Ring<T>) -> Option<T> {
        // SAFETY: each test pops from one thread only.
        unsafe { ring.try_pop() }
    }

    #[test]
    fn a_ring_is_a_power_of_two_in_segments_of_at_most_32() {
        for (capacity, cells, segments) in [
            (1, 1, 1),
            (3, 4, 1),
            (32, 32, 1),
            (33, 32, 2),
            (1000, 32, 32),
        ] {
            let ring = Ring::<u8>::new(capacity);
            assert_eq!(ring.per_segment(), cells, "capacity {capacity}");
            assert_eq!(ring.segment_mask + 1, segments, "capacity {capacity}");
        }
    }

    /// Lap after lap, the receiver keeping up within the capacity: every
    /// value in order, stamps of earlier laps never taken.
    #[test]
    fn values_come_out_in_order_lap_after_lap() {
        let ring = Ring::new(5);
        let mut next = 0u64;
        for lap in 0..40u64 {
            let n = 1 + lap % 5;
            for i in 0..n {
                ring.push(next + i);
            }
            for i in 0..n {
                assert_eq!(pop(&ring), Some(next + i));
            }
            assert_eq!(
                pop(&ring),
                None,
                "a stamp of an earlier lap is not this one's"
            );
            next += n;
        }
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn a_value_left_in_the_ring_is_dropped_once_with_it() {
        struct Counted(Arc<AtomicUsize>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let ring = Ring::new(64);
        for _ in 0..50 {
            ring.push(Counted(Arc::clone(&drops)));
        }
        for _ in 0..20 {
            assert!(pop(&ring).is_some());
        }
        for _ in 0..30 {
            ring.push(Counted(Arc::clone(&drops)));
        }
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 20);
        drop(ring);
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 80);
    }

    #[test]
    fn values_that_borrow_go_through() {
        let words = [String::from("a"), String::from("b")];
        let ring = Ring::new(8);
        for _ in 0..20 {
            ring.push(&words[0]);
            ring.push(&words[1]);
            assert_eq!(pop(&ring).map(String::as_str), Some("a"));
            assert_eq!(pop(&ring).map(String::as_str), Some("b"));
        }
    }

    /// Several pushers, admitted by a count of free cells as the channel's
    /// semaphore admits them, the receiver giving each back after its take:
    /// every value once and in each pusher's order, over many laps.
    #[test]
    fn concurrent_pushes_within_the_capacity_each_arrive_once_and_in_order() {
        const PUSHERS: u64 = 4;
        const CAPACITY: usize = 6;
        let each: u64 = if cfg!(miri) { 40 } else { 20_000 };
        let ring = Arc::new(Ring::<(u64, u64)>::new(CAPACITY));
        let free = Arc::new(AtomicUsize::new(CAPACITY));
        let pushers: Vec<_> = (0..PUSHERS)
            .map(|p| {
                let (ring, free) = (Arc::clone(&ring), Arc::clone(&free));
                std::thread::spawn(move || {
                    for i in 0..each {
                        // The semaphore's admission, as a counter.
                        loop {
                            let n = free.load(std::sync::atomic::Ordering::Acquire);
                            if n > 0
                                && free
                                    .compare_exchange(
                                        n,
                                        n - 1,
                                        std::sync::atomic::Ordering::AcqRel,
                                        std::sync::atomic::Ordering::Acquire,
                                    )
                                    .is_ok()
                            {
                                break;
                            }
                            std::hint::spin_loop();
                        }
                        ring.push((p, i));
                    }
                })
            })
            .collect();
        let mut next = [0u64; PUSHERS as usize];
        let mut got = 0;
        while got < PUSHERS * each {
            match pop(&ring) {
                Some((p, i)) => {
                    assert_eq!(i, next[p as usize], "pusher {p} out of order");
                    next[p as usize] += 1;
                    got += 1;
                    free.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
                None => std::hint::spin_loop(),
            }
        }
        for p in pushers {
            p.join().unwrap();
        }
        assert_eq!(pop(&ring), None);
    }
}
