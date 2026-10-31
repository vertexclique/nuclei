//! Where a file's call leaves its end for the task that waits on it: a
//! [`Slot`] the ending thread fills and wakes, polled through a [`SlotJob`].
//! A ring's operations end there (`ring`), and so do the writes the blocking
//! pool makes, on every backend.
//!
//! A write is counted on the shard of the thread that issued it from its
//! issue until its end was left and its wake delivered ([`WriteCall`]): a
//! task woken by that wake after its shard stopped is dropped within the
//! wake, so a runtime's drop that waits until its workers' counts are
//! naught, and their inboxes quiet, meets every linger such a task's
//! dropped handle hands on (`rt::linger`, proofs/tla/going/Linger.tla).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Poll, Waker};
use std::io;
use std::sync::Arc;

#[cfg(ring)]
use crate::buf::PoolBuf;
use crate::rt::shard::Shared;
use crate::sync::atomic_waker::AtomicWaker;

/// How a file's call ended, with its buffer.
pub(crate) enum FileOutcome {
    /// A read ends in a slot only on a ring.
    #[cfg(ring)]
    Read(io::Result<PoolBuf>),
    #[cfg(ring)]
    ReadLent(io::Result<usize>, Vec<u8>),
    Write(io::Result<()>),
    WriteLent(io::Result<()>, Vec<u8>),
    /// A write at the end of a file opened to append, and where the file's
    /// offset is after it.
    Append(io::Result<u64>),
    AppendLent(io::Result<u64>, Vec<u8>),
}

/// Where a call's end waits for its task.
pub(crate) struct Slot {
    full: AtomicBool,
    outcome: UnsafeCell<Option<FileOutcome>>,
    waker: AtomicWaker,
}

// SAFETY: the outcome is written once, before `full` is raised, by the
// thread that ends the call, and taken only after `full` is seen.
unsafe impl Send for Slot {}
// SAFETY: as for `Send`.
unsafe impl Sync for Slot {}

/// Slots a thread keeps for its next calls.
const SPARE_SLOTS: usize = 8;

thread_local! {
    /// Slots of this thread's ended calls. One is used again only once the
    /// thread that ended its call let go of it too: in steady state a call
    /// allocates no slot.
    static SPARES: core::cell::RefCell<Vec<Arc<Slot>>> =
        const { core::cell::RefCell::new(Vec::new()) };
}

impl Slot {
    fn fresh() -> Self {
        Self {
            full: AtomicBool::new(false),
            outcome: UnsafeCell::new(None),
            waker: AtomicWaker::new(),
        }
    }

    /// A slot for a new call: a spare nobody else holds, made fresh, or a
    /// new one.
    pub(crate) fn new() -> Arc<Self> {
        let spare = SPARES.try_with(|spares| {
            let mut spares = spares.try_borrow_mut().ok()?;
            let at = spares
                .iter_mut()
                .position(|slot| Arc::get_mut(slot).is_some())?;
            let mut slot = spares.swap_remove(at);
            // Unique (`get_mut` is), so nothing else reaches it: an outcome
            // or waker left from its last call goes here.
            if let Some(only) = Arc::get_mut(&mut slot) {
                *only = Self::fresh();
            }
            Some(slot)
        });
        spare
            .ok()
            .flatten()
            .unwrap_or_else(|| Arc::new(Self::fresh()))
    }

    /// Keeps `slot`, whose task is done with it, for a later call.
    fn spare(slot: Arc<Self>) {
        let _ = SPARES.try_with(|spares| {
            if let Ok(mut spares) = spares.try_borrow_mut()
                && spares.len() < SPARE_SLOTS
            {
                if spares.capacity() == 0 {
                    spares.reserve_exact(SPARE_SLOTS);
                }
                spares.push(slot);
            }
        });
    }

    /// Leaves the call's end and wakes its task.
    pub(crate) fn put(&self, outcome: FileOutcome) {
        // SAFETY: written once, before `full` is raised.
        unsafe { *self.outcome.get() = Some(outcome) };
        // Release: the task reads the outcome after it sees the flag.
        self.full.store(true, Ordering::Release);
        self.waker.wake();
    }

    /// The call's end, once it was left.
    pub(crate) fn take(&self) -> Option<FileOutcome> {
        // Acquire: pairs with the Release of `put`.
        if !self.full.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `full` is set: the ending thread is done with the outcome.
        unsafe { (*self.outcome.get()).take() }
    }
}

/// A lent read's end: its result, and the task's buffer back.
#[cfg(ring)]
pub(crate) type LentReadEnd = (io::Result<usize>, Vec<u8>);
/// A lent write's end: its result, and the task's buffer back.
pub(crate) type LentWriteEnd = (io::Result<()>, Vec<u8>);
/// A lent append's end: where the file's offset is after it, and the task's
/// buffer back.
pub(crate) type LentAppendEnd = (io::Result<u64>, Vec<u8>);

/// A file's call in flight, as the task that waits for it sees it.
pub(crate) struct SlotJob<T> {
    /// `None` once dropped: it went to this thread's spares.
    slot: Option<Arc<Slot>>,
    map: fn(FileOutcome) -> T,
}

impl<T> SlotJob<T> {
    /// The job of the call that ends in `slot`, its end taken by `map`.
    pub(crate) fn new(slot: Arc<Slot>, map: fn(FileOutcome) -> T) -> Self {
        Self {
            slot: Some(slot),
            map,
        }
    }

    /// The call's end once it came, else `Pending` with `waker` woken then.
    pub(crate) fn poll(&mut self, waker: &Waker) -> Poll<T> {
        let Some(slot) = &self.slot else {
            return Poll::Pending;
        };
        if let Some(outcome) = slot.take() {
            return Poll::Ready((self.map)(outcome));
        }
        slot.waker.register(waker);
        match slot.take() {
            Some(outcome) => Poll::Ready((self.map)(outcome)),
            None => Poll::Pending,
        }
    }
}

impl<T> Drop for SlotJob<T> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            Slot::spare(slot);
        }
    }
}

/// A file write counted on the shard whose thread issued it, until its end
/// was left and its wake delivered.
pub(crate) struct WriteCall(Arc<Shared>);

impl WriteCall {
    /// Counts a write on `shard`.
    pub(crate) fn on(shard: &Arc<Shared>) -> Self {
        shard.write_began();
        Self(Arc::clone(shard))
    }

    /// Counts a write on the shard this thread runs, if it runs one.
    pub(crate) fn here() -> Option<Self> {
        crate::rt::context::current(|current| current.map(|shard| Self::on(&shard.shared)))
    }
}

impl Drop for WriteCall {
    fn drop(&mut self) {
        self.0.write_ended();
    }
}

/// Makes `call` on the blocking pool of the runtime this thread is inside
/// and leaves its end in a slot, a write counted (see [`WriteCall`]) until
/// then. `waker` is registered before the call can end, so its end wakes it
/// whenever it comes.
pub(crate) fn write_on_pool<T, F>(call: F, map: fn(FileOutcome) -> T, waker: &Waker) -> SlotJob<T>
where
    F: FnOnce() -> FileOutcome + Send + 'static,
{
    let slot = Slot::new();
    slot.waker.register(waker);
    let write = WriteCall::here();
    let end = Arc::clone(&slot);
    crate::runtime::spawn_blocking(move || {
        end.put(call());
        // Once the wake went: a task it dropped handed what it left first.
        drop(write);
    })
    .detach();
    SlotJob::new(slot, map)
}

/// The ends as each kind of call takes them; an end of another kind cannot
/// come (each call is made with its own), and says so if it does.
#[cfg(ring)]
pub(crate) fn as_read(outcome: FileOutcome) -> io::Result<PoolBuf> {
    match outcome {
        FileOutcome::Read(result) => result,
        _ => Err(io::Error::other("a file read ended as another call")),
    }
}

#[cfg(ring)]
pub(crate) fn as_read_lent(outcome: FileOutcome) -> LentReadEnd {
    match outcome {
        FileOutcome::ReadLent(result, buf) => (result, buf),
        _ => (
            Err(io::Error::other("a file read ended as another call")),
            Vec::new(),
        ),
    }
}

pub(crate) fn as_write(outcome: FileOutcome) -> io::Result<()> {
    match outcome {
        FileOutcome::Write(result) => result,
        _ => Err(io::Error::other("a file write ended as another call")),
    }
}

pub(crate) fn as_write_lent(outcome: FileOutcome) -> LentWriteEnd {
    match outcome {
        FileOutcome::WriteLent(result, buf) => (result, buf),
        _ => (
            Err(io::Error::other("a file write ended as another call")),
            Vec::new(),
        ),
    }
}

pub(crate) fn as_append(outcome: FileOutcome) -> io::Result<u64> {
    match outcome {
        FileOutcome::Append(result) => result,
        _ => Err(io::Error::other("a file write ended as another call")),
    }
}

pub(crate) fn as_append_lent(outcome: FileOutcome) -> LentAppendEnd {
    match outcome {
        FileOutcome::AppendLent(result, buf) => (result, buf),
        _ => (
            Err(io::Error::other("a file write ended as another call")),
            Vec::new(),
        ),
    }
}
