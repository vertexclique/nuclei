//! The message queues behind a channel, each built for its one receiver and
//! wait-free for the senders and the receiver both: a ring for a bounded
//! channel whose capacity it holds (`ring`), a radix directory of leaves for
//! an unbounded one and a larger bounded one (`radix`). Each sits behind
//! [`MessageQueue`] so a model-checking build can swap them.
//!
//! The protocol the channel relies on is small:
//!
//! - Values come out in the order their pushes claimed positions. A push that
//!   returned before another began claimed the earlier position, so one
//!   sender's values come out in the order it sent them.
//! - A `push` that has returned is visible to every `try_pop` that starts
//!   after it.
//! - A value is taken at most once, and a value still queued when the queue
//!   is dropped is dropped exactly once.
//! - [`try_pop`](MessageQueue::try_pop) never waits. When the next position
//!   is claimed by a push that has not written its value yet, it answers
//!   `None`, as for an empty queue, and takes nothing: the receiver registers
//!   its waker and returns `Pending`, and the pusher wakes it after its push
//!   returns, as it does for every push. (A sender that is still writing still
//!   counts as a sender, so the receiver never reads that as the end.)
//! - A push's mark (a stamp, a ready flag) is stored, and the receiver's look
//!   at it loaded, sequentially consistently: the queue's half of the
//!   channel's wake handshake (see `chan`).
//!
//! # Under loom
//!
//! kovan is not instrumented, so loom cannot interleave the queues' steps.
//! Under `--cfg loom` the queue is a row of cells on loom's atomics, with no
//! segments and no lock: a push claims the next cell with a fetch-add, writes
//! its value and marks it ready, as the real ones do. So a receiver can find
//! the next cell claimed and empty, and the models check the handshake the
//! channel builds around that answer (the waker, the flag, the counts, the
//! close bit), and count how often their search met it. proofs/tla/chanring and
//! proofs/tla/radix check the queues' own steps, the radix directory's
//! reclamation included.

#![allow(unsafe_code)]

#[cfg(not(all(loom, test)))]
mod radix;
#[cfg(not(all(loom, test)))]
mod ring;

#[cfg(not(all(loom, test)))]
pub(super) use self::radix::Radix;

/// What a channel needs of its queue.
pub(super) trait MessageQueue<T> {
    /// Appends `value`.
    fn push(&self, value: T);

    /// Takes the oldest value, without waiting: `None` when there is none
    /// yet, or when the next one is still being written.
    ///
    /// # Safety
    ///
    /// The caller is the queue's one receiver: no two calls of this function
    /// on one queue run at once.
    unsafe fn try_pop(&self) -> Option<T>;

    /// How many values are queued, as a snapshot: a push in progress counts,
    /// a pop in progress does not.
    fn len(&self) -> usize;
}

/// A bounded channel's queue: the ring when the capacity fits it (the ring's
/// segment table is made whole at once), the radix directory past that.
#[cfg(not(all(loom, test)))]
pub(super) enum Bounded<T> {
    Ring(ring::Ring<T>),
    Radix(Radix<T>),
}

#[cfg(not(all(loom, test)))]
impl<T> Bounded<T> {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        if (capacity as u64) <= ring::MAX_CELLS {
            Self::Ring(ring::Ring::new(capacity))
        } else {
            Self::Radix(Radix::new())
        }
    }
}

#[cfg(not(all(loom, test)))]
impl<T> MessageQueue<T> for Bounded<T> {
    #[inline]
    fn push(&self, value: T) {
        match self {
            Self::Ring(ring) => ring.push(value),
            Self::Radix(radix) => radix.push(value),
        }
    }

    #[inline]
    unsafe fn try_pop(&self) -> Option<T> {
        match self {
            // SAFETY: the caller's contract, passed on.
            Self::Ring(ring) => unsafe { ring.try_pop() },
            // SAFETY: as above.
            Self::Radix(radix) => unsafe { radix.try_pop() },
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Ring(ring) => ring.len(),
            Self::Radix(radix) => radix.len(),
        }
    }
}

#[cfg(not(all(loom, test)))]
impl<T> MessageQueue<T> for Radix<T> {
    #[inline]
    fn push(&self, value: T) {
        Radix::push(self, value);
    }

    #[inline]
    unsafe fn try_pop(&self) -> Option<T> {
        // SAFETY: the caller's contract, passed on.
        unsafe { Radix::try_pop(self) }
    }

    fn len(&self) -> usize {
        Radix::len(self)
    }
}

/// How many values one loom model's channel may ever take in: its models
/// send a handful.
#[cfg(all(loom, test))]
const LOOM_CELLS: usize = 8;

/// A cell of the loom model.
#[cfg(all(loom, test))]
struct LoomCell<T> {
    /// The value is written, as the real queue's mark.
    ready: crate::sync::internal::AtomicBool,
    /// A push claimed the cell: kept outside loom's view (a plain std atomic),
    /// so counting the receiver's looks at a cell being written adds no
    /// access the model checks.
    claimed: std::sync::atomic::AtomicBool,
    value: crate::sync::internal::UnsafeCell<Option<T>>,
}

/// The queue of one channel, modelled for loom with atomics alone, the steps
/// of the real queue without its segments: a row of cells, a push claiming
/// the next with a fetch-add, writing its value and marking it ready (both
/// sequentially consistent, as there), and the receiver's position its own.
#[cfg(all(loom, test))]
pub(super) struct Queue<T> {
    tail: crate::sync::internal::AtomicU64,
    head: crate::sync::internal::AtomicU64,
    cells: [LoomCell<T>; LOOM_CELLS],
}

// SAFETY: as for the real queue: values move between threads, which is all
// `T: Send` allows; the receiver's end is reached by one thread at a time
// (`try_pop`'s contract), and every shared word is an atomic.
#[cfg(all(loom, test))]
unsafe impl<T: Send> Send for Queue<T> {}
// SAFETY: as above.
#[cfg(all(loom, test))]
unsafe impl<T: Send> Sync for Queue<T> {}

#[cfg(all(loom, test))]
impl<T> Queue<T> {
    pub(super) fn new() -> Self {
        Self {
            tail: crate::sync::internal::AtomicU64::new(0),
            head: crate::sync::internal::AtomicU64::new(0),
            cells: core::array::from_fn(|_| LoomCell {
                ready: crate::sync::internal::AtomicBool::new(false),
                claimed: std::sync::atomic::AtomicBool::new(false),
                value: crate::sync::internal::UnsafeCell::new(None),
            }),
        }
    }

    /// Claims the next cell, then writes it, in two steps.
    pub(super) fn push(&self, value: T) {
        use crate::sync::internal::Ordering;
        let pos = self.tail.fetch_add(1, Ordering::SeqCst) as usize;
        assert!(
            pos < LOOM_CELLS,
            "a loom model sent more than {LOOM_CELLS} values"
        );
        let cell = &self.cells[pos];
        cell.claimed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: the claim made the cell this push's alone until it is ready.
        cell.value.with_mut(|v| unsafe { *v = Some(value) });
        cell.ready.store(true, Ordering::SeqCst);
    }

    /// As the real queue's: `None` for an empty queue and for a next cell
    /// claimed and not written yet (counted for the models).
    ///
    /// # Safety
    ///
    /// The caller is the queue's one receiver, as for the real queue.
    pub(super) unsafe fn try_pop(&self) -> Option<T> {
        use crate::sync::internal::Ordering;
        let pos = self.head.load(Ordering::Relaxed) as usize;
        let cell = self.cells.get(pos)?;
        if !cell.ready.load(Ordering::SeqCst) {
            if cell.claimed.load(std::sync::atomic::Ordering::Relaxed) {
                crate::sync::loom_model::busy_look();
            }
            return None;
        }
        // SAFETY: ready: its push wrote it; the receiver takes it once.
        let value = cell.value.with_mut(|v| unsafe { (*v).take() });
        self.head.store(pos as u64 + 1, Ordering::Relaxed);
        value
    }

    pub(super) fn len(&self) -> usize {
        use crate::sync::internal::Ordering;
        let front = self.head.load(Ordering::Acquire);
        let back = self.tail.load(Ordering::Acquire);
        back.saturating_sub(front) as usize
    }
}

/// Under loom both flavors take the model.
#[cfg(all(loom, test))]
pub(super) type Radix<T> = Queue<T>;
/// As above.
#[cfg(all(loom, test))]
pub(super) type Bounded<T> = Queue<T>;

#[cfg(all(loom, test))]
impl<T> Queue<T> {
    /// The model for a bounded channel: the capacity is the semaphore's.
    pub(super) fn with_capacity(_capacity: usize) -> Self {
        Self::new()
    }
}

#[cfg(all(loom, test))]
impl<T> MessageQueue<T> for Queue<T> {
    fn push(&self, value: T) {
        Queue::push(self, value);
    }

    unsafe fn try_pop(&self) -> Option<T> {
        // SAFETY: the caller's contract, passed on.
        unsafe { Queue::try_pop(self) }
    }

    fn len(&self) -> usize {
        Queue::len(self)
    }
}
