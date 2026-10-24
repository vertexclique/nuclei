//! An asynchronous reader-writer lock with an upgradable read; how it
//! admits waiters is described on its core, `raw_rwlock`.

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::ops::Deref;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use super::internal;
use super::mutex::TryLockError;
use super::raw_rwlock::{
    MAX_READERS, READ, RawRwLock, RwWait, UPGRADABLE_READ, UpgradeWait, WRITE,
};

/// The most readers [`RwLock::with_max_readers`] accepts: `u32::MAX >> 3`,
/// or the reader field of the lock where a word is too narrow for that.
const MAX_READS: usize = if (u32::MAX >> 3) as usize <= MAX_READERS {
    (u32::MAX >> 3) as usize
} else {
    MAX_READERS
};

/// A reader-writer lock whose waits are futures.
///
/// Uncontended, a read or write lock and its release are one atomic
/// read-modify-write each and allocate nothing. Under contention a caller
/// takes the lock whenever it is free to it, even past queued waiters, so
/// a release never waits for a suspended task; a waiter passed over
/// [`MAX_BYPASS`](super::MAX_BYPASS) times is handed the lock next, and
/// nobody gets in ahead of it meanwhile, so neither readers nor writers
/// starve.
///
/// [`upgradable_read`](Self::upgradable_read) takes a read that can later
/// become the write without letting a writer in between. One upgradable
/// read exists at a time; it shares the lock with plain readers, and plain
/// read guards cannot upgrade. A write guard becomes a read guard, with no
/// writer in between, through
/// [`RwLockWriteGuard::downgrade`].
///
/// ```
/// use nuclei::sync::RwLock;
///
/// let lock = RwLock::new(5);
/// let a = lock.try_read().expect("free");
/// let b = lock.try_read().expect("readers share");
/// assert!(lock.try_write().is_err());
/// drop((a, b));
/// *lock.try_write().expect("free again") += 1;
/// assert_eq!(lock.into_inner(), 6);
/// ```
pub struct RwLock<T: ?Sized> {
    raw: RawRwLock,
    value: core::cell::UnsafeCell<T>,
}

// SAFETY: the value moves with the lock.
unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
// SAFETY: readers share `&T` across threads and a writer gets `&mut T`
// alone, the same contract as `std::sync::RwLock`.
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    loom_const_fn! {
        /// An unlocked lock holding `value`.
        pub fn new(value: T) -> Self {
            Self { raw: RawRwLock::new(), value: core::cell::UnsafeCell::new(value) }
        }
    }

    loom_const_fn! {
        /// An unlocked lock holding `value`, usable in a `static`.
        pub fn const_new(value: T) -> Self {
            Self { raw: RawRwLock::new(), value: core::cell::UnsafeCell::new(value) }
        }
    }

    loom_const_fn! {
        /// An unlocked lock holding `value` that admits at most `max_reads`
        /// readers at once: a reader that finds them all inside waits for
        /// one to leave, as one held back by a writer does.
        ///
        /// # Panics
        ///
        /// When `max_reads` is `0` or is bigger than `u32::MAX >> 3`. (A
        /// target whose word is narrower than 64 bits caps it lower, at the
        /// readers its state word can count: `2^23 - 1` on 32 bits.)
        pub fn with_max_readers(value: T, max_reads: u32) -> Self {
            assert!(max_reads != 0, "a RwLock may not be created with 0 readers");
            assert!(
                max_reads as usize <= MAX_READS,
                "a RwLock may not be created with more than u32::MAX >> 3 readers"
            );
            Self {
                raw: RawRwLock::with_max_readers(max_reads as usize),
                value: core::cell::UnsafeCell::new(value),
            }
        }
    }

    loom_const_fn! {
        /// [`with_max_readers`](Self::with_max_readers), usable in a
        /// `static`.
        ///
        /// # Panics
        ///
        /// As for [`with_max_readers`](Self::with_max_readers).
        pub fn const_with_max_readers(value: T, max_reads: u32) -> Self {
            assert!(max_reads != 0, "a RwLock may not be created with 0 readers");
            assert!(
                max_reads as usize <= MAX_READS,
                "a RwLock may not be created with more than u32::MAX >> 3 readers"
            );
            Self {
                raw: RawRwLock::with_max_readers(max_reads as usize),
                value: core::cell::UnsafeCell::new(value),
            }
        }
    }

    loom_const_fn! {
        /// An unlocked lock holding `value` whose contended acquires never
        /// spin: they queue at once. For a lock its holders keep across long
        /// waits of their own (I/O, say), where a spin could only burn the
        /// caller's time.
        pub(crate) fn new_without_spin(value: T) -> Self {
            Self { raw: RawRwLock::without_spin(), value: core::cell::UnsafeCell::new(value) }
        }
    }

    /// Consumes the lock and returns its value.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

#[cfg(all(test, not(loom)))]
impl<T: ?Sized> RwLock<T> {
    /// The rounds a contended acquire of this lock would spin now.
    pub(crate) fn spin_rounds(&self) -> u32 {
        self.raw.spin_rounds()
    }
}

impl<T: ?Sized> RwLock<T> {
    /// Shares the lock if no writer holds it, no upgrade is under way, the
    /// readers have room and no handoff is owed.
    ///
    /// # Errors
    ///
    /// [`TryLockError`] when any of those fails.
    pub fn try_read(&self) -> Result<RwLockReadGuard<'_, T>, TryLockError> {
        if self.raw.try_kind(READ) {
            Ok(RwLockReadGuard::new(self))
        } else {
            Err(TryLockError(()))
        }
    }

    /// [`try_read`](Self::try_read) through an `Arc`, so the guard can
    /// outlive the borrow.
    ///
    /// # Errors
    ///
    /// [`TryLockError`] when the lock cannot be shared now.
    pub fn try_read_owned(self: Arc<Self>) -> Result<OwnedRwLockReadGuard<T>, TryLockError> {
        if self.raw.try_kind(READ) {
            Ok(OwnedRwLockReadGuard::new(self))
        } else {
            Err(TryLockError(()))
        }
    }

    /// Takes the lock alone if nobody holds it and no handoff is owed.
    ///
    /// # Errors
    ///
    /// [`TryLockError`] when any reader or writer holds the lock, or a
    /// waiter is owed it.
    pub fn try_write(&self) -> Result<RwLockWriteGuard<'_, T>, TryLockError> {
        if self.raw.try_kind(WRITE) {
            Ok(RwLockWriteGuard::new(self))
        } else {
            Err(TryLockError(()))
        }
    }

    /// [`try_write`](Self::try_write) through an `Arc`, so the guard can
    /// outlive the borrow.
    ///
    /// # Errors
    ///
    /// [`TryLockError`] when the lock cannot be taken alone now.
    pub fn try_write_owned(self: Arc<Self>) -> Result<OwnedRwLockWriteGuard<T>, TryLockError> {
        if self.raw.try_kind(WRITE) {
            Ok(OwnedRwLockWriteGuard::new(self))
        } else {
            Err(TryLockError(()))
        }
    }

    /// Takes the upgradable read if no writer and no other upgradable read
    /// holds the lock and no handoff is owed.
    pub fn try_upgradable_read(&self) -> Option<RwLockUpgradableReadGuard<'_, T>> {
        self.raw
            .try_kind(UPGRADABLE_READ)
            .then(|| RwLockUpgradableReadGuard { lock: self })
    }

    /// Waits to share the lock. Uncontended, the first poll is one atomic
    /// read-modify-write. Contended, it is lock-free, not wait-free: see
    /// [Progress](super#progress).
    ///
    /// Dropping the future withdraws the wait, and passes on a lock handed
    /// to it meanwhile.
    pub fn read(&self) -> Read<'_, T> {
        Read {
            lock: self,
            wait: RwWait::new(),
        }
    }

    /// [`read`](Self::read) through an `Arc`, so the guard can outlive the
    /// borrow.
    pub fn read_owned(self: Arc<Self>) -> ReadOwned<T> {
        ReadOwned {
            lock: self,
            wait: RwWait::new(),
        }
    }

    /// Waits to hold the lock alone. Uncontended, the first poll is one atomic
    /// read-modify-write. Contended, it is lock-free, not wait-free: see
    /// [Progress](super#progress).
    ///
    /// Dropping the future withdraws the wait, and passes on a lock handed
    /// to it meanwhile.
    pub fn write(&self) -> Write<'_, T> {
        Write {
            lock: self,
            wait: RwWait::new(),
        }
    }

    /// Waits to hold the lock alone, with a guard that keeps the lock alive
    /// itself, so the future and the guard can outlive the borrow that made
    /// them: a wait kept across calls, or a guard returned to a caller.
    pub fn write_owned(self: Arc<Self>) -> WriteOwned<T> {
        WriteOwned {
            lock: self,
            wait: RwWait::new(),
        }
    }

    /// [`read`](Self::read) for a thread that may block: it parks the
    /// calling thread until the lock can be shared. On a nuclei shard (inside one of its tasks) it does not park the thread: it runs a nested `block_on`, so the shard keeps running its other tasks meanwhile.
    /// Inside another runtime's task it blocks that task's executor thread,
    /// so do not: the task that would release may be the one that thread was
    /// running.
    ///
    /// # Panics
    ///
    /// On `wasm32`, where no other thread can release, when the lock cannot
    /// be shared at once: the call could never end.
    pub fn blocking_read(&self) -> RwLockReadGuard<'_, T> {
        internal::blocking(self.read())
    }

    /// [`write`](Self::write) for a thread that may block: see
    /// [`blocking_read`](Self::blocking_read).
    ///
    /// # Panics
    ///
    /// On `wasm32`, where no other thread can release, when the lock is held
    /// at all: the call could never end.
    pub fn blocking_write(&self) -> RwLockWriteGuard<'_, T> {
        internal::blocking(self.write())
    }

    /// Waits for the upgradable read. It makes the progress
    /// [`read`](Self::read) does.
    pub fn upgradable_read(&self) -> UpgradableRead<'_, T> {
        UpgradableRead {
            lock: self,
            wait: RwWait::new(),
        }
    }

    /// The value, borrowed mutably; `&mut self` proves no guard exists.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

impl<T> From<T> for RwLock<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("RwLock");
        match self.try_read() {
            Ok(inner) => d.field("data", &&*inner),
            Err(_) => d.field("data", &format_args!("<locked>")),
        };
        d.finish()
    }
}

/// The upgradable read of a [`RwLock`]: shared access that can become
/// exclusive without a writer getting in between. Releases on drop, with
/// the progress [`RwLockReadGuard`]'s release makes.
///
/// It moves to another thread only when `T` is `Send` as well as `Sync`:
/// an upgrade turns it into `&mut T`, so moving it can move the value. A
/// value that may be shared but not moved, such as a
/// [`std::sync::MutexGuard`], keeps the guard on its thread:
///
/// ```compile_fail,E0277
/// use nuclei::sync::RwLockUpgradableReadGuard;
/// use std::sync::MutexGuard;
///
/// fn moves<T: Send>() {}
/// moves::<RwLockUpgradableReadGuard<'static, Option<MutexGuard<'static, ()>>>>();
/// ```
#[must_use = "dropping the guard releases the lock at once"]
pub struct RwLockUpgradableReadGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
}

// SAFETY: moving it moves `&T` access and, through `upgrade` and
// `try_upgrade`, `&mut T` access, as moving the `Upgrade` future does.
unsafe impl<T: ?Sized + Send + Sync> Send for RwLockUpgradableReadGuard<'_, T> {}
// SAFETY: `&RwLockUpgradableReadGuard` reaches `&T` only: both upgrades
// take the guard by value.
unsafe impl<T: ?Sized + Sync> Sync for RwLockUpgradableReadGuard<'_, T> {}

impl<'a, T: ?Sized> RwLockUpgradableReadGuard<'a, T> {
    /// Waits for the readers inside to leave, holding new readers back
    /// meanwhile, then holds the write.
    pub fn upgrade(self) -> Upgrade<'a, T> {
        let lock = self.lock;
        core::mem::forget(self);
        Upgrade {
            lock,
            wait: UpgradeWait::new(),
            done: false,
        }
    }

    /// Holds the write at once if no reader is inside; otherwise hands the
    /// upgradable read back.
    pub fn try_upgrade(self) -> Result<RwLockWriteGuard<'a, T>, Self> {
        if !self.lock.raw.try_upgrade() {
            return Err(self);
        }
        let lock = self.lock;
        core::mem::forget(self);
        Ok(RwLockWriteGuard::new(lock))
    }
}

impl<T: ?Sized> Deref for RwLockUpgradableReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the upgradable read excludes every writer.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for RwLockUpgradableReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.raw.upgradable_unlock();
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockUpgradableReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// The future [`RwLock::read`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct Read<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    wait: RwWait,
}

impl<'a, T: ?Sized> Future for Read<'a, T> {
    type Output = RwLockReadGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<RwLockReadGuard<'a, T>> {
        let this = self.get_mut();
        this.wait
            .poll(&this.lock.raw, READ, cx.waker())
            .map(|()| RwLockReadGuard::new(this.lock))
    }
}

impl<T: ?Sized> Drop for Read<'_, T> {
    fn drop(&mut self) {
        self.wait.cancel(&self.lock.raw, READ);
    }
}

impl<T: ?Sized> fmt::Debug for Read<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Read").finish_non_exhaustive()
    }
}

/// The future [`RwLock::read_owned`] returns. It keeps its place in the
/// queue, and the bypasses it counted, for as long as it lives; dropping it
/// withdraws the wait and passes on a lock handed to it meanwhile.
#[must_use = "futures do nothing unless polled"]
pub struct ReadOwned<T: ?Sized> {
    lock: Arc<RwLock<T>>,
    wait: RwWait,
}

impl<T: ?Sized> Future for ReadOwned<T> {
    type Output = OwnedRwLockReadGuard<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<OwnedRwLockReadGuard<T>> {
        let this = self.get_mut();
        this.wait
            .poll(&this.lock.raw, READ, cx.waker())
            .map(|()| OwnedRwLockReadGuard::new(Arc::clone(&this.lock)))
    }
}

impl<T: ?Sized> Drop for ReadOwned<T> {
    fn drop(&mut self) {
        self.wait.cancel(&self.lock.raw, READ);
    }
}

impl<T: ?Sized> fmt::Debug for ReadOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadOwned").finish_non_exhaustive()
    }
}

/// The future [`RwLock::write`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct Write<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    wait: RwWait,
}

impl<'a, T: ?Sized> Future for Write<'a, T> {
    type Output = RwLockWriteGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<RwLockWriteGuard<'a, T>> {
        let this = self.get_mut();
        this.wait
            .poll(&this.lock.raw, WRITE, cx.waker())
            .map(|()| RwLockWriteGuard::new(this.lock))
    }
}

impl<T: ?Sized> Drop for Write<'_, T> {
    fn drop(&mut self) {
        self.wait.cancel(&self.lock.raw, WRITE);
    }
}

impl<T: ?Sized> fmt::Debug for Write<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Write").finish_non_exhaustive()
    }
}

/// The future [`RwLock::write_owned`] returns. It keeps its place in the
/// queue, and the bypasses it counted, for as long as it lives; dropping it
/// withdraws the wait and passes on a handoff made to it meanwhile.
#[must_use = "futures do nothing unless polled"]
pub struct WriteOwned<T: ?Sized> {
    lock: Arc<RwLock<T>>,
    wait: RwWait,
}

impl<T: ?Sized> Future for WriteOwned<T> {
    type Output = OwnedRwLockWriteGuard<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<OwnedRwLockWriteGuard<T>> {
        let this = self.get_mut();
        this.wait
            .poll(&this.lock.raw, WRITE, cx.waker())
            .map(|()| OwnedRwLockWriteGuard::new(Arc::clone(&this.lock)))
    }
}

impl<T: ?Sized> Drop for WriteOwned<T> {
    fn drop(&mut self) {
        self.wait.cancel(&self.lock.raw, WRITE);
    }
}

impl<T: ?Sized> fmt::Debug for WriteOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteOwned").finish_non_exhaustive()
    }
}

/// The future [`RwLock::upgradable_read`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct UpgradableRead<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    wait: RwWait,
}

impl<'a, T: ?Sized> Future for UpgradableRead<'a, T> {
    type Output = RwLockUpgradableReadGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<RwLockUpgradableReadGuard<'a, T>> {
        let this = self.get_mut();
        this.wait
            .poll(&this.lock.raw, UPGRADABLE_READ, cx.waker())
            .map(|()| RwLockUpgradableReadGuard { lock: this.lock })
    }
}

impl<T: ?Sized> Drop for UpgradableRead<'_, T> {
    fn drop(&mut self) {
        self.wait.cancel(&self.lock.raw, UPGRADABLE_READ);
    }
}

impl<T: ?Sized> fmt::Debug for UpgradableRead<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpgradableRead").finish_non_exhaustive()
    }
}

/// The future [`RwLockUpgradableReadGuard::upgrade`] returns. Dropping it
/// before it completes releases the upgradable read.
#[must_use = "futures do nothing unless polled"]
pub struct Upgrade<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    wait: UpgradeWait,
    done: bool,
}

// SAFETY: it holds the upgradable read and yields the write: moving it
// moves `&mut T` access, sharing it shares nothing.
unsafe impl<T: ?Sized + Send + Sync> Send for Upgrade<'_, T> {}
// SAFETY: `&Upgrade` exposes nothing.
unsafe impl<T: ?Sized + Send + Sync> Sync for Upgrade<'_, T> {}

impl<'a, T: ?Sized> Future for Upgrade<'a, T> {
    type Output = RwLockWriteGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<RwLockWriteGuard<'a, T>> {
        let this = self.get_mut();
        assert!(
            !this.done,
            "an Upgrade future was polled after it completed"
        );
        this.wait.poll(&this.lock.raw, cx.waker()).map(|()| {
            this.done = true;
            RwLockWriteGuard::new(this.lock)
        })
    }
}

impl<T: ?Sized> Drop for Upgrade<'_, T> {
    fn drop(&mut self) {
        if !self.done {
            self.wait.cancel(&self.lock.raw);
        }
    }
}

impl<T: ?Sized> fmt::Debug for Upgrade<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upgrade")
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

mod guards;
pub use guards::{
    OwnedRwLockMappedWriteGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard,
    RwLockMappedWriteGuard, RwLockReadGuard, RwLockWriteGuard,
};

#[cfg(all(test, not(loom)))]
mod tests;
