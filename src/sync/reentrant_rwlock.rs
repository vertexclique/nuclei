//! An asynchronous reader-writer lock its holders may lock again, keyed
//! by [`Owner`].
//!
//! The lock counts owners, not calls. An owner that already reads reads
//! again at once, even with a writer owed a handoff, because that writer
//! waits for this owner's read anyway; an owner that writes may also read.
//! An owner that reads without writing and asks to write is refused with
//! [`ReadHeldByOwner`]: the write would wait for its own read. So an owner
//! never deadlocks on itself in a request it polls: every poll looks at the
//! owner's write and reads first, queued or not, so a request that queued
//! before its owner came to hold the lock through another one (two requests
//! in one `join!`) completes at its next poll, and the entry that made the
//! owner a holder wakes the owner's other queued requests on the lock, so
//! none waits for a lock only its own owner can free.
//!
//! Each reading owner has a record (owner id, read depth) in a lock-free
//! list that only grows: a record is reused once its owner stops reading,
//! so the list is as long as the most owners that ever read at once.

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::ops::Deref;
use core::pin::Pin;
use core::ptr;
use core::task::{Context, Poll};

use super::internal::{AtomicPtr, AtomicU64, AtomicUsize, Ordering, UnsafeCell};
use super::owner::Owner;
use super::raw_rwlock::{READ, RawRwLock, RwWait, WRITE};
use super::siblings::{Ticket, wake_siblings};

/// One reading owner: its id (zero when free) and how many reads it holds.
struct Record {
    owner: AtomicU64,
    reads: AtomicUsize,
    /// Set before the record is published and never again.
    next: *mut Record,
}

struct Records {
    head: AtomicPtr<Record>,
}

impl Records {
    loom_const_fn! {
        fn new() -> Self {
            Self { head: AtomicPtr::new(ptr::null_mut()) }
        }
    }

    fn iter(&self) -> impl Iterator<Item = &Record> {
        let mut cursor = self.head.load(Ordering::Acquire);
        core::iter::from_fn(move || {
            // SAFETY: records are freed only when the list is dropped.
            let record = unsafe { cursor.as_ref() }?;
            cursor = record.next;
            Some(record)
        })
    }

    /// The record `id` holds. Only an owner stores its own id, so only
    /// that owner can find it.
    fn find(&self, id: u64) -> Option<&Record> {
        self.iter()
            .find(|record| record.owner.load(Ordering::Relaxed) == id)
    }

    /// A free record, now held by `id`.
    fn claim(&self, id: u64) -> &Record {
        if let Some(record) = self.iter().find(|record| {
            record.owner.load(Ordering::Relaxed) == 0
                && record
                    .owner
                    .compare_exchange(0, id, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
        }) {
            return record;
        }
        let fresh = Box::into_raw(Box::new(Record {
            owner: AtomicU64::new(id),
            reads: AtomicUsize::new(0),
            next: ptr::null_mut(),
        }));
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: `fresh` is unpublished, so this thread owns it.
            unsafe { (*fresh).next = head };
            match self
                .head
                .compare_exchange_weak(head, fresh, Ordering::Release, Ordering::Relaxed)
            {
                // SAFETY: published; it lives until the list is dropped.
                Ok(_) => return unsafe { &*fresh },
                Err(actual) => head = actual,
            }
        }
    }
}

impl Drop for Records {
    fn drop(&mut self) {
        let mut cursor = self.head.load(Ordering::Acquire);
        while !cursor.is_null() {
            // SAFETY: `&mut self` leaves the list unreachable to others.
            let record = unsafe { Box::from_raw(cursor) };
            cursor = record.next;
        }
    }
}

/// A reader-writer lock its holders may lock again, with the bounded
/// bypass of [`RwLock`](super::RwLock).
///
/// Both guards give `&T`, since one owner may hold several at once; the
/// write side excludes every other owner, so mutate through interior
/// mutability that is `Sync` (an atomic, a [`Mutex`](super::Mutex)), as
/// readers on other threads share the value. Guards and lock futures
/// borrow their [`Owner`] and stay on its thread.
///
/// ```
/// use nuclei::sync::{Owner, ReentrantRwLock};
/// use std::sync::atomic::{AtomicU32, Ordering};
///
/// let lock = ReentrantRwLock::new(AtomicU32::new(0));
/// let me = Owner::new();
/// let write = lock.try_write(&me).expect("free");
/// let read = lock.try_read(&me).expect("a writer may also read");
/// write.store(7, Ordering::Relaxed);
/// drop(write);
/// assert!(lock.try_write(&Owner::new()).is_none(), "the read is still held");
/// assert_eq!(read.load(Ordering::Relaxed), 7);
/// ```
pub struct ReentrantRwLock<T: ?Sized> {
    raw: RawRwLock,
    /// The writing owner's id, or zero; only the writer stores its own id.
    writer: AtomicU64,
    /// How many write guards the writer holds; touched only by the writer.
    write_depth: UnsafeCell<usize>,
    readers: Records,
    value: core::cell::UnsafeCell<T>,
}

// SAFETY: the value moves with the lock.
unsafe impl<T: ?Sized + Send> Send for ReentrantRwLock<T> {}
// SAFETY: readers on several threads share `&T`, so `T: Sync`; the writer
// gets nothing more than `&T`.
unsafe impl<T: ?Sized + Send + Sync> Sync for ReentrantRwLock<T> {}

impl<T> ReentrantRwLock<T> {
    loom_const_fn! {
        /// An unlocked lock holding `value`.
        pub fn new(value: T) -> Self {
            Self {
                raw: RawRwLock::new(),
                writer: AtomicU64::new(0),
                write_depth: UnsafeCell::new(0),
                readers: Records::new(),
                value: core::cell::UnsafeCell::new(value),
            }
        }
    }

    /// Consumes the lock and returns its value.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> ReentrantRwLock<T> {
    /// Reads if `owner` already reads or writes, or if no writer holds the
    /// lock and no handoff is owed.
    pub fn try_read<'a>(&'a self, owner: &'a Owner) -> Option<ReentrantReadGuard<'a, T>> {
        let record = match self.reenter_read(owner, 0) {
            Some(record) => record,
            None if self.raw.try_kind(READ) => self.enter_read(owner, 0),
            None => return None,
        };
        Some(ReentrantReadGuard::new(self, owner, record))
    }

    /// Writes if `owner` already writes, or if nobody holds the lock and
    /// no handoff is owed. `None` too while `owner` reads without writing.
    pub fn try_write<'a>(&'a self, owner: &'a Owner) -> Option<ReentrantWriteGuard<'a, T>> {
        if self.reenter_write(owner) {
            return Some(ReentrantWriteGuard::new(self, owner));
        }
        (self.readers.find(owner.id()).is_none() && self.raw.try_kind(WRITE)).then(|| {
            self.enter_write(owner, 0);
            ReentrantWriteGuard::new(self, owner)
        })
    }

    /// Waits to read, at once if `owner` already reads or writes. A
    /// contended wait is lock-free, not wait-free: see
    /// [Progress](super#progress).
    pub fn read<'a>(&'a self, owner: &'a Owner) -> ReentrantRead<'a, T> {
        ReentrantRead {
            lock: self,
            owner,
            wait: RwWait::new(),
            ticket: Ticket::new(),
        }
    }

    /// Waits to write, at once if `owner` already writes. Fails with
    /// [`ReadHeldByOwner`] when `owner` reads without writing: the write
    /// would wait for that read, which only `owner` can end. A contended
    /// wait is lock-free, not wait-free: see [Progress](super#progress).
    pub fn write<'a>(&'a self, owner: &'a Owner) -> ReentrantWrite<'a, T> {
        ReentrantWrite {
            lock: self,
            owner,
            wait: RwWait::new(),
            ticket: Ticket::new(),
        }
    }

    /// The value, borrowed mutably; `&mut self` proves no guard exists.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    fn reenter_read(&self, owner: &Owner, except: u64) -> Option<&Record> {
        let id = owner.id();
        if let Some(record) = self.readers.find(id) {
            record.reads.fetch_add(1, Ordering::Relaxed);
            return Some(record);
        }
        (self.writer.load(Ordering::Relaxed) == id).then(|| self.enter_read(owner, except))
    }

    /// Records a first read by `owner`, which holds a share or the write,
    /// and wakes its other queued requests on this lock (all but ticket
    /// `except`).
    fn enter_read(&self, owner: &Owner, except: u64) -> &Record {
        let record = self.readers.claim(owner.id());
        record.reads.store(1, Ordering::Relaxed);
        wake_siblings(self.address(), owner.id(), except);
        record
    }

    /// The lock's identity in the registry of queued requests.
    fn address(&self) -> usize {
        core::ptr::from_ref(self).cast::<()>() as usize
    }

    fn read_unlock(&self, id: u64, record: &Record) {
        if record.reads.fetch_sub(1, Ordering::Relaxed) == 1 {
            record.owner.store(0, Ordering::Release);
            // While the owner writes, its reads hold no share of their own.
            if self.writer.load(Ordering::Relaxed) != id {
                self.raw.read_unlock();
            }
        }
    }

    #[expect(
        clippy::panic,
        reason = "a count that cannot overflow in practice, checked rather than wrapped"
    )]
    fn reenter_write(&self, owner: &Owner) -> bool {
        if self.writer.load(Ordering::Relaxed) != owner.id() {
            return false;
        }
        // SAFETY: only the writer touches the depth, and this is it.
        self.write_depth.with_mut(|depth| unsafe {
            *depth = (*depth)
                .checked_add(1)
                .unwrap_or_else(|| panic!("a ReentrantRwLock was write-locked usize::MAX times"));
        });
        true
    }

    /// Records `owner` as the writer, and wakes its other queued requests on
    /// this lock (all but ticket `except`).
    fn enter_write(&self, owner: &Owner, except: u64) {
        // SAFETY: the write side makes this caller the only writer.
        self.write_depth.with_mut(|depth| unsafe { *depth = 1 });
        self.writer.store(owner.id(), Ordering::Relaxed);
        wake_siblings(self.address(), owner.id(), except);
    }

    fn write_unlock(&self, id: u64) {
        // SAFETY: only the writer drops a write guard.
        let left = self.write_depth.with_mut(|depth| unsafe {
            *depth -= 1;
            *depth
        });
        if left == 0 {
            self.writer.store(0, Ordering::Relaxed);
            if self.readers.find(id).is_some() {
                self.raw.downgrade();
            } else {
                self.raw.write_unlock();
            }
        }
    }
}

impl<T: Default> Default for ReentrantRwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: ?Sized> fmt::Debug for ReentrantRwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReentrantRwLock").finish_non_exhaustive()
    }
}

/// One read of a [`ReentrantRwLock`]; drops one level of the owner's
/// reads. The drop of the last level releases the read, which is
/// lock-free, not wait-free: with waiters it returns after one pass over
/// the queue, plus at most one for each operation of another thread that
/// lands on the queue meanwhile (see [Progress](super#progress)).
#[must_use = "dropping the guard releases one read at once"]
pub struct ReentrantReadGuard<'a, T: ?Sized> {
    lock: &'a ReentrantRwLock<T>,
    owner: &'a Owner,
    record: &'a Record,
}

impl<'a, T: ?Sized> ReentrantReadGuard<'a, T> {
    fn new(lock: &'a ReentrantRwLock<T>, owner: &'a Owner, record: &'a Record) -> Self {
        Self {
            lock,
            owner,
            record,
        }
    }
}

impl<T: ?Sized> Deref for ReentrantReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: a read excludes every other owner's write, and the
        // writer, if it is this owner, only ever has `&T` too.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for ReentrantReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.read_unlock(self.owner.id(), self.record);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for ReentrantReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// One write of a [`ReentrantRwLock`]; drops one level of the owner's
/// writes. The drop of the last level releases the write, with the
/// progress [`ReentrantReadGuard`]'s release makes.
#[must_use = "dropping the guard releases one write at once"]
pub struct ReentrantWriteGuard<'a, T: ?Sized> {
    lock: &'a ReentrantRwLock<T>,
    owner: &'a Owner,
}

impl<'a, T: ?Sized> ReentrantWriteGuard<'a, T> {
    fn new(lock: &'a ReentrantRwLock<T>, owner: &'a Owner) -> Self {
        Self { lock, owner }
    }
}

impl<T: ?Sized> Deref for ReentrantWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the write excludes every other owner, and this owner's
        // guards only ever hand out `&T`.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for ReentrantWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.write_unlock(self.owner.id());
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for ReentrantWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// The future [`ReentrantRwLock::read`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct ReentrantRead<'a, T: ?Sized> {
    lock: &'a ReentrantRwLock<T>,
    owner: &'a Owner,
    wait: RwWait,
    /// Its place among the owner's queued requests.
    ticket: Ticket,
}

impl<'a, T: ?Sized> Future for ReentrantRead<'a, T> {
    type Output = ReentrantReadGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<ReentrantReadGuard<'a, T>> {
        let this = self.get_mut();
        if let Some(record) = this.lock.reenter_read(this.owner, this.ticket.number()) {
            // The owner came to read or write through another request since
            // this one queued: this one leaves the queue, passing on a share
            // it was granted meanwhile, so the owner keeps one record.
            // The guard first: the cancel and the budget's refusal run user
            // code, and a panic there gives the record back.
            let guard = ReentrantReadGuard::new(this.lock, this.owner, record);
            if this.wait.is_queued() {
                this.wait.cancel(&this.lock.raw, READ);
            }
            this.ticket.unregister();
            // Completed at once: it spends one operation of the poll's
            // budget, or, the budget spent, the read goes and it yields.
            if crate::coop::spend(cx.waker()) {
                return Poll::Ready(guard);
            }
            drop(guard);
            return Poll::Pending;
        }
        match this.wait.poll(&this.lock.raw, READ, cx.waker()) {
            Poll::Ready(()) => {
                let record = this.lock.enter_read(this.owner, this.ticket.number());
                this.ticket.unregister();
                Poll::Ready(ReentrantReadGuard::new(this.lock, this.owner, record))
            }
            Poll::Pending => {
                if this.wait.is_queued() {
                    this.ticket
                        .register(this.lock.address(), this.owner.id(), cx.waker());
                }
                Poll::Pending
            }
        }
    }
}

impl<T: ?Sized> Drop for ReentrantRead<'_, T> {
    fn drop(&mut self) {
        self.wait.cancel(&self.lock.raw, READ);
    }
}

impl<T: ?Sized> fmt::Debug for ReentrantRead<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReentrantRead")
            .field("owner", self.owner)
            .finish_non_exhaustive()
    }
}

/// Why a [`ReentrantRwLock::write`] failed: its owner reads without
/// writing, so the write would wait for a read only that owner can end.
/// Release the reads first, or take the write before reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadHeldByOwner;

impl fmt::Display for ReadHeldByOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("this owner holds a read of the lock; release it before writing")
    }
}

impl std::error::Error for ReadHeldByOwner {}

/// The future [`ReentrantRwLock::write`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct ReentrantWrite<'a, T: ?Sized> {
    lock: &'a ReentrantRwLock<T>,
    owner: &'a Owner,
    wait: RwWait,
    /// Its place among the owner's queued requests.
    ticket: Ticket,
}

impl<'a, T: ?Sized> Future for ReentrantWrite<'a, T> {
    type Output = Result<ReentrantWriteGuard<'a, T>, ReadHeldByOwner>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        // Either completes at once: it spends one operation of the poll's
        // budget, or, the budget spent, the level goes and it yields. A request
        // that queued before its owner came to write or read through another
        // one leaves the queue first, which cannot have granted it the write.
        if this.lock.reenter_write(this.owner) {
            let guard = ReentrantWriteGuard::new(this.lock, this.owner);
            if this.wait.is_queued() {
                this.wait.cancel(&this.lock.raw, WRITE);
            }
            this.ticket.unregister();
            if crate::coop::spend(cx.waker()) {
                return Poll::Ready(Ok(guard));
            }
            drop(guard);
            return Poll::Pending;
        }
        if this.lock.readers.find(this.owner.id()).is_some() {
            if this.wait.is_queued() {
                this.wait.cancel(&this.lock.raw, WRITE);
            }
            this.ticket.unregister();
            return if crate::coop::spend(cx.waker()) {
                Poll::Ready(Err(ReadHeldByOwner))
            } else {
                Poll::Pending
            };
        }
        match this.wait.poll(&this.lock.raw, WRITE, cx.waker()) {
            Poll::Ready(()) => {
                this.lock.enter_write(this.owner, this.ticket.number());
                this.ticket.unregister();
                Poll::Ready(Ok(ReentrantWriteGuard::new(this.lock, this.owner)))
            }
            Poll::Pending => {
                if this.wait.is_queued() {
                    this.ticket
                        .register(this.lock.address(), this.owner.id(), cx.waker());
                }
                Poll::Pending
            }
        }
    }
}

impl<T: ?Sized> Drop for ReentrantWrite<'_, T> {
    fn drop(&mut self) {
        self.wait.cancel(&self.lock.raw, WRITE);
    }
}

impl<T: ?Sized> fmt::Debug for ReentrantWrite<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReentrantWrite")
            .field("owner", self.owner)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::MAX_BYPASS;
    use crate::sync::test_support::Polled;

    #[test]
    fn a_reader_reads_again_while_a_writer_waits() {
        let lock = ReentrantRwLock::new(());
        let (me, writer) = (Owner::new(), Owner::new());
        let first = lock.try_read(&me).expect("free");
        let mut write = Polled::new(lock.write(&writer));
        write.pending();
        let second = Polled::new(lock.read(&me)).ready();
        drop(first);
        drop(second);
        assert!(write.wakes() >= 1, "the last read out nudges the writer");
        let held = write.ready().expect("the writer holds no read");
        assert!(lock.try_read(&Owner::new()).is_none());
        drop(held);
    }

    #[test]
    fn an_owned_read_never_waits_behind_an_owed_writer() {
        let lock = ReentrantRwLock::new(());
        let (me, writer) = (Owner::new(), Owner::new());
        let others: Vec<Owner> = (0..=MAX_BYPASS).map(|_| Owner::new()).collect();
        let mine = lock.try_read(&me).expect("free");
        let mut write = Polled::new(lock.write(&writer));
        write.pending();
        let mut other = lock.try_read(&others[0]).expect("barge");
        for owner in &others[1..] {
            let next = lock.try_read(owner).expect("a new owner barges");
            drop(other);
            other = next;
            write.pending();
        }
        assert!(lock.try_read(&Owner::new()).is_none(), "the writer is owed");
        let again = Polled::new(lock.read(&me)).ready();
        drop((other, again, mine));
        assert!(write.ready().is_ok());
    }

    #[test]
    fn a_writer_reads_and_keeps_its_read_after_the_write() {
        let lock = ReentrantRwLock::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let write = lock.try_write(&me).expect("free");
        let nested = lock.try_write(&me).expect("the writer re-enters");
        let read = lock.try_read(&me).expect("a writer may also read");
        drop((nested, write));
        assert!(
            lock.try_write(&other).is_none(),
            "the read survives the write"
        );
        let shared = lock.try_read(&other).expect("the write became a read");
        drop((read, shared));
        assert!(lock.try_write(&other).is_some());
    }

    fn records_of(lock: &ReentrantRwLock<()>, owner: &Owner) -> usize {
        lock.readers
            .iter()
            .filter(|record| record.owner.load(Ordering::Relaxed) == owner.id())
            .count()
    }

    #[test]
    fn a_queued_read_of_the_owner_that_took_the_write_meanwhile_reads_at_once() {
        let lock = ReentrantRwLock::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let theirs = lock.try_write(&other).expect("free");
        let mut write = Polled::new(lock.write(&me));
        let mut read = Polled::new(lock.read(&me));
        write.pending();
        read.pending();
        drop(theirs);
        let written = write.ready().expect("the owner held no read");
        let reading = read.ready();
        drop((written, reading));
        assert!(
            lock.try_write(&other).is_some(),
            "the write and the read went"
        );
    }

    #[test]
    fn a_queued_write_of_an_owner_that_reads_meanwhile_is_refused() {
        let lock = ReentrantRwLock::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let theirs = lock.try_read(&other).expect("free");
        let mut write = Polled::new(lock.write(&me));
        write.pending();
        let mine = lock.try_read(&me).expect("a queued writer is not owed yet");
        drop(theirs);
        assert_eq!(write.ready().err(), Some(ReadHeldByOwner));
        drop((write, mine));
        assert!(
            lock.try_write(&other).is_some(),
            "the refused write left no trace"
        );
    }

    #[test]
    fn two_queued_reads_of_one_owner_share_one_record() {
        let lock = ReentrantRwLock::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let theirs = lock.try_write(&other).expect("free");
        let mut first = Polled::new(lock.read(&me));
        let mut second = Polled::new(lock.read(&me));
        first.pending();
        second.pending();
        drop(theirs);
        let a = first.ready();
        let b = second.ready();
        assert_eq!(
            records_of(&lock, &me),
            1,
            "one record for the owner's two reads"
        );
        drop((a, b));
        assert!(lock.try_write(&other).is_some(), "both reads went");
    }

    #[test]
    fn a_reader_asking_to_write_is_refused_rather_than_deadlocked() {
        let lock = ReentrantRwLock::new(());
        let me = Owner::new();
        let read = lock.try_read(&me).expect("free");
        assert!(lock.try_write(&me).is_none());
        assert_eq!(
            Polled::new(lock.write(&me)).ready().err(),
            Some(ReadHeldByOwner)
        );
        drop(read);
        assert!(Polled::new(lock.write(&me)).ready().is_ok());
    }

    #[test]
    fn a_write_entry_wakes_the_read_request_of_its_owner_queued_behind_it() {
        let lock = ReentrantRwLock::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let theirs = lock.try_write(&other).expect("free");
        let mut write = Polled::new(lock.write(&me));
        let mut read = Polled::new(lock.read(&me));
        write.pending();
        read.pending();
        drop(theirs);
        read.pending();
        assert_eq!(read.wakes(), 0);
        let writing = write.ready().expect("the owner reads nothing");
        assert_eq!(read.wakes(), 1, "the owner's entry woke its other request");
        let reading = read.ready();
        drop((writing, reading));
        assert!(lock.try_write(&other).is_some());
    }

    #[test]
    fn a_read_entry_wakes_the_read_request_of_its_owner_queued_behind_a_writer() {
        let lock = ReentrantRwLock::new(());
        let (me, other, third) = (Owner::new(), Owner::new(), Owner::new());
        let theirs = lock.try_write(&other).expect("free");
        let mut elder = Polled::new(lock.read(&me));
        let mut writer = Polled::new(lock.write(&third));
        let mut younger = Polled::new(lock.read(&me));
        elder.pending();
        writer.pending();
        younger.pending();
        drop(theirs);
        younger.pending();
        let first = elder.ready();
        assert_eq!(
            younger.wakes(),
            1,
            "the owner's entry woke its other request"
        );
        let second = younger.ready();
        drop((first, second));
        assert!(writer.poll().is_ready(), "the writer gets its turn");
    }

    #[test]
    fn a_read_entry_wakes_the_write_request_of_its_owner_to_fail() {
        let lock = ReentrantRwLock::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let theirs = lock.try_write(&other).expect("free");
        let mut read = Polled::new(lock.read(&me));
        let mut write = Polled::new(lock.write(&me));
        read.pending();
        write.pending();
        drop(theirs);
        write.pending();
        let reading = read.ready();
        assert_eq!(write.wakes(), 1);
        assert!(matches!(write.ready(), Err(ReadHeldByOwner)));
        drop(reading);
    }
}
