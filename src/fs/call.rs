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
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Poll, Waker};
use std::io;
use std::sync::Arc;

use crate::buf::PoolBuf;
use crate::buf::owned::OwnedBuf;
use crate::rt::shard::Shared;
use crate::sync::atomic_waker::AtomicWaker;

/// How a file's call ended, with its buffer.
pub(crate) enum FileOutcome {
    /// A read ends in a slot only on a ring.
    #[cfg(ring)]
    Read(io::Result<PoolBuf>),
    #[cfg(ring)]
    ReadLent(io::Result<usize>, OwnedBuf),
    Write(io::Result<()>),
    /// A chunk of a copy into the file, and how many bytes it moved.
    Copied(io::Result<u64>),
    WriteLent(io::Result<()>, OwnedBuf),
    /// A write at the end of a file opened to append, and where the file's
    /// offset is after it.
    Append(io::Result<u64>),
    AppendLent(io::Result<u64>, OwnedBuf),
    /// A write from a buffer of the pool that comes back with its end.
    WriteKept(io::Result<()>, PoolBuf),
    /// As `Append`, with the buffer of the pool back.
    AppendKept(io::Result<u64>, PoolBuf),
    /// A sync ends in a slot only on a ring.
    #[cfg(ring)]
    Sync(io::Result<()>),
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

    /// Wakes `waker` when the call's end is left.
    #[cfg(ring)]
    pub(crate) fn register(&self, waker: &Waker) {
        self.waker.register(waker);
    }

    /// Leaves the call's end and wakes its task.
    #[cfg(ring)]
    pub(crate) fn put(&self, outcome: FileOutcome) {
        if let Some(waker) = self.fill(outcome) {
            waker.wake();
        }
    }

    /// Leaves the call's end, and takes out the waker its task left, to
    /// wake.
    fn fill(&self, outcome: FileOutcome) -> Option<Waker> {
        // SAFETY: written once, before `full` is raised.
        unsafe { *self.outcome.get() = Some(outcome) };
        // Release: the task reads the outcome after it sees the flag.
        self.full.store(true, Ordering::Release);
        self.waker.take()
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
pub(crate) type LentReadEnd = (io::Result<usize>, OwnedBuf);
/// A lent write's end: its result, and the task's buffer back.
pub(crate) type LentWriteEnd = (io::Result<()>, OwnedBuf);
/// A lent append's end: where the file's offset is after it, and the task's
/// buffer back.
pub(crate) type LentAppendEnd = (io::Result<u64>, OwnedBuf);
/// A kept write's end: its result, and the buffer of the pool back (none
/// only for an end of another kind than its call's).
pub(crate) type KeptWriteEnd = (io::Result<()>, Option<PoolBuf>);
/// A kept append's end: where the file's offset is after it, and the buffer
/// of the pool back (none as for [`KeptWriteEnd`]).
pub(crate) type KeptAppendEnd = (io::Result<u64>, Option<PoolBuf>);

/// A file's call in flight, as the task that waits for it sees it: its end
/// is taken as `T` (see [`End`]).
pub(crate) struct SlotJob<T> {
    /// `None` once dropped: it went to this thread's spares.
    slot: Option<Arc<Slot>>,
    end: PhantomData<fn() -> T>,
}

impl<T> SlotJob<T>
where
    T: End,
{
    /// The job of the call that ends in `slot`.
    pub(crate) fn new(slot: Arc<Slot>) -> Self {
        Self {
            slot: Some(slot),
            end: PhantomData,
        }
    }

    /// The call's end once it came, else `Pending` with `waker` woken then.
    pub(crate) fn poll(&mut self, waker: &Waker) -> Poll<T> {
        let Some(slot) = &self.slot else {
            return Poll::Pending;
        };
        if let Some(outcome) = slot.take() {
            return Poll::Ready(T::of(outcome));
        }
        slot.waker.register(waker);
        match slot.take() {
            Some(outcome) => Poll::Ready(T::of(outcome)),
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

    /// Counts a write on the shard this thread runs, if it runs one; else on the shard its poll
    /// lost to the handoff, whose task the rest of the poll is and whose stopped state a
    /// runtime's drop waits on (`Shared::settled`).
    pub(crate) fn here() -> Option<Self> {
        crate::rt::context::current(|current| current.map(|shard| Self::on(&shard.shared)))
            .or_else(|| crate::rt::context::lost_shared(|lost| lost.map(Self::on)))
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
pub(crate) fn write_on_pool<T, F>(call: F, waker: &Waker) -> SlotJob<T>
where
    T: End,
    F: FnOnce() -> FileOutcome + Send + 'static,
{
    let slot = Slot::new();
    slot.waker.register(waker);
    let write = WriteCall::here();
    let end = Arc::clone(&slot);
    crate::runtime::spawn_blocking(move || Ending::new(end, call(), write)).detach();
    SlotJob::new(slot)
}

/// The end of a call a blocking job made: `outcome`, for `slot`, and the
/// write counted until its wake went. The job returns it, and it is
/// published as it drops: a detached job's run drops what it returns once
/// the job's task is gone (`RawTask::run`), so the task woken finds that
/// task's block back with the thread it came from; off the pool, at once.
pub(crate) struct Ending {
    end: Option<(Arc<Slot>, FileOutcome)>,
    write: Option<WriteCall>,
}

impl Ending {
    /// The end `outcome` of the call of `slot`, with `write` counted until
    /// its wake went.
    pub(crate) fn new(slot: Arc<Slot>, outcome: FileOutcome, write: Option<WriteCall>) -> Self {
        Self {
            end: Some((slot, outcome)),
            write,
        }
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        let Some((slot, outcome)) = self.end.take() else {
            return;
        };
        let waker = slot.fill(outcome);
        // The thread's count goes before the wake: the slot is a spare the
        // task can take again once it is woken.
        drop(slot);
        if let Some(waker) = waker {
            waker.wake();
        }
        // Once the wake went: a task it dropped handed what it left first.
        drop(self.write.take());
    }
}

/// The end a kind of call takes out of its slot: each call is made with
/// its own kind of end, so an end of another kind cannot come, and says so
/// if it does.
pub(crate) trait End: Sized {
    fn of(outcome: FileOutcome) -> Self;
}

/// An end of another kind than its call's.
#[cold]
fn mismatched() -> io::Error {
    io::Error::other("a file call ended as another kind of call")
}

/// A read into a buffer of the pool, which ends in a slot only on a ring.
impl End for io::Result<PoolBuf> {
    #[inline]
    fn of(outcome: FileOutcome) -> Self {
        match outcome {
            #[cfg(ring)]
            FileOutcome::Read(result) => result,
            _ => Err(mismatched()),
        }
    }
}

/// A write, a range's change or a socket's shutdown, and on a ring a sync.
impl End for io::Result<()> {
    #[inline]
    fn of(outcome: FileOutcome) -> Self {
        match outcome {
            FileOutcome::Write(result) => result,
            #[cfg(ring)]
            FileOutcome::Sync(result) => result,
            _ => Err(mismatched()),
        }
    }
}

/// An append, ending where the file's offset is after it, or a chunk of a
/// copy, ending with its count.
impl End for io::Result<u64> {
    #[inline]
    fn of(outcome: FileOutcome) -> Self {
        match outcome {
            FileOutcome::Append(result) | FileOutcome::Copied(result) => result,
            _ => Err(mismatched()),
        }
    }
}

/// A read into a task's buffer, which ends in a slot only on a ring.
impl End for LentReadEnd {
    #[inline]
    fn of(outcome: FileOutcome) -> Self {
        match outcome {
            #[cfg(ring)]
            FileOutcome::ReadLent(result, buf) => (result, buf),
            _ => (Err(mismatched()), OwnedBuf::default()),
        }
    }
}

impl End for LentWriteEnd {
    #[inline]
    fn of(outcome: FileOutcome) -> Self {
        match outcome {
            FileOutcome::WriteLent(result, buf) => (result, buf),
            _ => (Err(mismatched()), OwnedBuf::default()),
        }
    }
}

impl End for LentAppendEnd {
    #[inline]
    fn of(outcome: FileOutcome) -> Self {
        match outcome {
            FileOutcome::AppendLent(result, buf) => (result, buf),
            _ => (Err(mismatched()), OwnedBuf::default()),
        }
    }
}

impl End for KeptWriteEnd {
    #[inline]
    fn of(outcome: FileOutcome) -> Self {
        match outcome {
            FileOutcome::WriteKept(result, buf) => (result, Some(buf)),
            // A call given up on has no buffer to bring back.
            FileOutcome::Write(Err(err)) => (Err(err), None),
            _ => (Err(mismatched()), None),
        }
    }
}

impl End for KeptAppendEnd {
    #[inline]
    fn of(outcome: FileOutcome) -> Self {
        match outcome {
            FileOutcome::AppendKept(result, buf) => (result, Some(buf)),
            _ => (Err(mismatched()), None),
        }
    }
}
