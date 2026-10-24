//! Userspace views of the two shared rings.
//!
//! [`Sq`] is the producer side of the submission ring: it caches the tail locally, writes
//! entries into the SQE array and publishes the tail with a release store. [`Cq`] is the
//! consumer side of the completion ring: [`Cq::completions`] reads the tail once with acquire,
//! yields copies of the entries and releases the head when the iterator is dropped. Both hold
//! raw pointers into mappings owned by the [`Ring`](super::Ring) that built them and must not
//! outlive it.

use std::{
    error::Error,
    fmt, ptr,
    sync::atomic::{AtomicU32, Ordering, fence},
};

use super::{
    sqe::Sqe,
    sys::{
        IORING_SQ_CQ_OVERFLOW, IORING_SQ_NEED_WAKEUP, IoCqringOffsets, IoSqringOffsets, IoUringCqe,
        IoUringSqe,
    },
};

/// The submission queue has no free slot; submit and try again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SqFull;

impl fmt::Display for SqFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the submission queue is full")
    }
}

impl Error for SqFull {}

/// Producer side of the submission ring.
#[derive(Debug)]
pub(crate) struct Sq {
    khead: *const AtomicU32,
    ktail: *const AtomicU32,
    kflags: *const AtomicU32,
    sqes: *mut IoUringSqe,
    mask: u32,
    entries: u32,
    /// Tail as this thread sees it: the slot the next push fills. Equal to the kernel-visible
    /// tail after every [`Sq::publish`].
    tail: u32,
}

impl Sq {
    /// Builds the view over a mapped submission ring.
    ///
    /// # Safety
    /// `ring` must be the base of a live ring mapping whose layout `off` describes, and `sqes`
    /// the base of the live SQE array of `entries` slots (a power of two). Both mappings must
    /// outlive the returned value. When `off.array` is non-zero the index array is filled with
    /// the identity map, so the caller must not share it with another writer.
    pub(crate) unsafe fn new(
        ring: *mut u8,
        off: &IoSqringOffsets,
        sqes: *mut IoUringSqe,
        entries: u32,
        has_array: bool,
    ) -> Self {
        let at = |o: u32| -> *const AtomicU32 {
            // SAFETY: the caller guarantees `o` is an offset inside the live ring mapping.
            unsafe { ring.add(o as usize) }.cast::<AtomicU32>()
        };
        if has_array {
            let array = at(off.array).cast_mut().cast::<u32>();
            for i in 0..entries {
                // SAFETY: the array holds `entries` u32 slots inside the mapping, per the contract.
                unsafe { array.add(i as usize).write(i) };
            }
        }
        let ktail = at(off.tail);
        Self {
            khead: at(off.head),
            ktail,
            kflags: at(off.flags),
            sqes,
            mask: entries - 1,
            entries,
            // SAFETY: `ktail` points into the live mapping.
            // Relaxed: the ring is fresh, no other thread or kernel writer exists yet.
            tail: unsafe { &*ktail }.load(Ordering::Relaxed),
        }
    }

    fn head(&self) -> &AtomicU32 {
        // SAFETY: `khead` points into the ring mapping, which outlives `self` (see `new`).
        unsafe { &*self.khead }
    }

    fn flags(&self) -> &AtomicU32 {
        // SAFETY: `kflags` points into the ring mapping, which outlives `self` (see `new`).
        unsafe { &*self.kflags }
    }

    /// Entries pushed since the kernel last consumed any (includes unpublished ones).
    pub(crate) fn ready(&self) -> u32 {
        // Acquire: pairs with the kernel's release store of head, so a slot it freed is no longer read.
        self.tail.wrapping_sub(self.head().load(Ordering::Acquire))
    }

    /// Free slots left.
    pub(crate) fn space(&self) -> u32 {
        self.entries - self.ready()
    }

    /// Copies `sqe` into the next slot; it reaches the kernel at the next [`Sq::publish`] plus enter.
    pub(crate) fn push(&mut self, sqe: &Sqe) -> Result<(), SqFull> {
        if self.ready() >= self.entries {
            return Err(SqFull);
        }
        // SAFETY: `tail & mask` is below `entries`, inside the SQE array, and the free-slot check
        // above says the kernel has finished reading that slot.
        unsafe { ptr::write(self.sqes.add((self.tail & self.mask) as usize), sqe.0) };
        self.tail = self.tail.wrapping_add(1);
        Ok(())
    }

    /// Makes every pushed entry visible to the kernel.
    pub(crate) fn publish(&mut self) {
        // SAFETY: `ktail` points into the ring mapping, which outlives `self` (see `new`).
        // Release: the entry writes above must be visible before the kernel can see the new tail.
        unsafe { &*self.ktail }.store(self.tail, Ordering::Release);
    }

    /// For a ring with a kernel submission thread, after [`Sq::publish`]: whether the thread has
    /// gone to sleep and needs `IORING_ENTER_SQ_WAKEUP`.
    pub(crate) fn sqpoll_needs_wakeup(&self) -> bool {
        // SeqCst fence: orders the tail store before the flags load, matching the barrier the
        // polling thread runs between setting the flag and re-checking the tail, so a wakeup is not missed.
        fence(Ordering::SeqCst);
        self.sqpoll_asleep()
    }

    /// Whether the kernel holds completions that did not fit the completion ring.
    pub(crate) fn cq_overflow_pending(&self) -> bool {
        // Relaxed: only a hint; the enter that flushes the backlog synchronises with the kernel.
        self.flags().load(Ordering::Relaxed) & IORING_SQ_CQ_OVERFLOW != 0
    }

    /// Whether the polling thread has set its sleeping flag (no fence: a status read, not a handshake).
    pub(crate) fn sqpoll_asleep(&self) -> bool {
        // Relaxed: ordering, where it matters, comes from the fence in `sqpoll_needs_wakeup`.
        self.flags().load(Ordering::Relaxed) & IORING_SQ_NEED_WAKEUP != 0
    }
}

/// Consumer side of the completion ring.
#[derive(Debug)]
pub(crate) struct Cq {
    khead: *const AtomicU32,
    ktail: *const AtomicU32,
    cqes: *const IoUringCqe,
    mask: u32,
}

impl Cq {
    /// Builds the view over a mapped completion ring.
    ///
    /// # Safety
    /// `ring` must be the base of a live ring mapping whose layout `off` describes, with
    /// `entries` completion slots (a power of two); the mapping must outlive the returned value.
    pub(crate) unsafe fn new(ring: *mut u8, off: &IoCqringOffsets, entries: u32) -> Self {
        let at = |o: u32| -> *const AtomicU32 {
            // SAFETY: the caller guarantees `o` is an offset inside the live ring mapping.
            unsafe { ring.add(o as usize) }.cast::<AtomicU32>()
        };
        Self {
            khead: at(off.head),
            ktail: at(off.tail),
            // SAFETY: `cqes` is an offset inside the live ring mapping.
            cqes: unsafe { ring.add(off.cqes as usize) }.cast::<IoUringCqe>(),
            mask: entries - 1,
        }
    }

    /// Drains the completions that have arrived. The tail is read once, with acquire, so entries
    /// that land during iteration wait for the next call. Dropping the iterator, finished or
    /// not, returns the consumed slots to the kernel with a release store of the head.
    pub(crate) fn completions(&mut self) -> Completions<'_> {
        // SAFETY: both pointers point into the ring mapping, which outlives `self` (see `new`).
        let (khead, ktail) = unsafe { (&*self.khead, &*self.ktail) };
        // Relaxed: this thread is the only writer of head.
        let head = khead.load(Ordering::Relaxed);
        // Acquire: pairs with the kernel's release store of tail, making the entry contents visible.
        let tail = ktail.load(Ordering::Acquire);
        Completions {
            cq: self,
            start: head,
            head,
            tail,
        }
    }
}

/// Iterator over the completions present when [`Cq::completions`] was called.
#[derive(Debug)]
pub(crate) struct Completions<'a> {
    cq: &'a Cq,
    start: u32,
    head: u32,
    tail: u32,
}

impl Iterator for Completions<'_> {
    type Item = IoUringCqe;

    fn next(&mut self) -> Option<IoUringCqe> {
        if self.head == self.tail {
            return None;
        }
        // SAFETY: `head & mask` is inside the CQE array, and the slot lies between the head and
        // the acquired tail, so the kernel has finished writing it and will not touch it until
        // we release the head.
        let cqe = unsafe { ptr::read(self.cq.cqes.add((self.head & self.cq.mask) as usize)) };
        self.head = self.head.wrapping_add(1);
        Some(cqe)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.tail.wrapping_sub(self.head) as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for Completions<'_> {}

impl Drop for Completions<'_> {
    fn drop(&mut self) {
        if self.head != self.start {
            // SAFETY: `khead` points into the ring mapping, which outlives the `Cq` borrowed here.
            // Release: our reads of the consumed entries must finish before the kernel may reuse the slots.
            unsafe { &*self.cq.khead }.store(self.head, Ordering::Release);
        }
    }
}
