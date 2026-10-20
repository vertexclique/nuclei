//! An asynchronous reader-writer lock with an upgradable read; how it
//! admits waiters is described on its core, `raw_rwlock`.

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::Arc;

use super::raw_rwlock::{READ, RawRwLock, RwWait, UPGRADABLE_READ, UpgradeWait, WRITE};

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
/// read guards cannot upgrade.
///
/// ```
/// use nuclei::sync::RwLock;
///
/// let lock = RwLock::new(5);
/// let a = lock.try_read().expect("free");
/// let b = lock.try_read().expect("readers share");
/// assert!(lock.try_write().is_none());
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
    /// Shares the lock if no writer holds it, no upgrade is under way and
    /// no handoff is owed.
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        self.raw.try_kind(READ).then(|| RwLockReadGuard {
            lock: self,
            _value: PhantomData,
        })
    }

    /// Takes the lock alone if nobody holds it and no handoff is owed.
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        self.raw.try_kind(WRITE).then(|| RwLockWriteGuard {
            lock: self,
            _value: PhantomData,
        })
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
    pub fn read(&self) -> Read<'_, T> {
        Read {
            lock: self,
            wait: RwWait::new(),
        }
    }

    /// Waits to hold the lock alone. Uncontended, the first poll is one atomic
    /// read-modify-write. Contended, it is lock-free, not wait-free: see
    /// [Progress](super#progress).
    pub fn write(&self) -> Write<'_, T> {
        Write {
            lock: self,
            wait: RwWait::new(),
        }
    }

    /// Waits to hold the lock alone, with a guard that keeps the lock alive
    /// itself, so the future and the guard can outlive the borrow that made
    /// them: a wait kept across calls, or a guard returned to a caller.
    pub fn write_owned(self: &Arc<Self>) -> WriteOwned<T> {
        WriteOwned {
            lock: Arc::clone(self),
            wait: RwWait::new(),
        }
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

impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: ?Sized> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RwLock").finish_non_exhaustive()
    }
}

/// Shared access to a [`RwLock`]'s value; releases on drop.
///
/// The release is one atomic read-modify-write when no waiter needs telling.
/// Otherwise it is lock-free, not wait-free: it returns after one pass over
/// the queue, plus at most one for each operation of another thread that
/// lands on the queue meanwhile (see [Progress](super#progress)).
#[must_use = "dropping the guard releases the lock at once"]
pub struct RwLockReadGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    _value: PhantomData<&'a T>,
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: a read excludes every writer.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.raw.read_unlock();
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// Exclusive access to a [`RwLock`]'s value; releases on drop. The release
/// makes the progress [`RwLockReadGuard`]'s does.
#[must_use = "dropping the guard releases the lock at once"]
pub struct RwLockWriteGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    _value: PhantomData<&'a mut T>,
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the write excludes every other guard.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.raw.write_unlock();
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// Exclusive access to a [`RwLock`]'s value through an [`Arc`] the guard
/// holds; releases on drop. Made by [`RwLock::write_owned`].
#[must_use = "dropping the guard releases the lock at once"]
pub struct OwnedRwLockWriteGuard<T: ?Sized> {
    lock: Arc<RwLock<T>>,
    // `Sync` only when `T: Sync`, since `&OwnedRwLockWriteGuard` reaches `&T`.
    _value: PhantomData<core::cell::UnsafeCell<T>>,
}

// SAFETY: sharing the guard shares `&T`, which needs `T: Sync`.
unsafe impl<T: ?Sized + Send + Sync> Sync for OwnedRwLockWriteGuard<T> {}

impl<T: ?Sized> OwnedRwLockWriteGuard<T> {
    /// The lock this guard holds.
    pub fn lock(&self) -> &Arc<RwLock<T>> {
        &self.lock
    }
}

impl<T: ?Sized> Deref for OwnedRwLockWriteGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the write excludes every other guard.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> DerefMut for OwnedRwLockWriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for OwnedRwLockWriteGuard<T> {
    fn drop(&mut self) {
        self.lock.raw.write_unlock();
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for OwnedRwLockWriteGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
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
        Ok(RwLockWriteGuard {
            lock,
            _value: PhantomData,
        })
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
            .map(|()| RwLockReadGuard {
                lock: this.lock,
                _value: PhantomData,
            })
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
            .map(|()| RwLockWriteGuard {
                lock: this.lock,
                _value: PhantomData,
            })
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
            .map(|()| OwnedRwLockWriteGuard {
                lock: Arc::clone(&this.lock),
                _value: PhantomData,
            })
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
            RwLockWriteGuard {
                lock: this.lock,
                _value: PhantomData,
            }
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

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::MAX_BYPASS;
    use crate::sync::test_support::{Polled, block_on};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicIsize, Ordering as StdOrdering};

    /// A lock built without spin queues its contended readers and writers at
    /// once: its budget is zero from the start and stays zero through a
    /// contended write, its wake and its retry, where a spinning lock's
    /// starts above zero.
    #[test]
    fn a_rwlock_built_without_spin_never_spins() {
        assert!(RwLock::new(()).spin_rounds() > 0, "a plain lock spins");
        let lock = RwLock::new_without_spin(0u8);
        assert_eq!(lock.spin_rounds(), 0);
        let read = lock.try_read().expect("free");
        let mut writer = Polled::new(lock.write());
        writer.pending();
        drop(read);
        assert_eq!(writer.wakes(), 1);
        *writer.ready() += 1;
        assert_eq!(lock.spin_rounds(), 0, "the lock never spins");
    }

    const ITERATIONS: usize = if cfg!(miri) { 20 } else { 2_000 };

    #[test]
    fn readers_share_and_a_writer_excludes() {
        let lock = RwLock::new(0);
        let a = lock.try_read().expect("free");
        let b = lock.try_read().expect("readers share");
        assert!(lock.try_write().is_none());
        drop((a, b));
        let w = lock.try_write().expect("free");
        assert!(lock.try_read().is_none());
        drop(w);
    }

    #[test]
    fn readers_barge_past_a_waiting_writer_until_it_is_owed() {
        let lock = RwLock::new(());
        let mut inside = lock.try_read().expect("free");
        let mut writer = Polled::new(lock.write());
        writer.pending();
        for round in 0..MAX_BYPASS {
            let next = lock.try_read().expect("a read barges past the writer");
            drop(inside);
            inside = next;
            assert_eq!(
                writer.wakes() as u32,
                round + 1,
                "every release nudges the writer"
            );
            writer.pending();
        }
        assert!(lock.try_read().is_none(), "the writer is owed the lock now");
        drop(inside);
        assert_eq!(writer.wakes() as u32, MAX_BYPASS + 1);
        drop(writer.ready());
        assert!(lock.try_read().is_some());
    }

    #[test]
    fn a_released_writer_lets_the_queued_readers_in_together() {
        let lock = RwLock::new(());
        let w0 = lock.try_write().expect("free");
        let mut r1 = Polled::new(lock.read());
        r1.pending();
        let mut r2 = Polled::new(lock.read());
        r2.pending();
        drop(w0);
        assert_eq!((r1.wakes(), r2.wakes()), (1, 1));
        let (a, b) = (r1.ready(), r2.ready());
        drop((a, b));
    }

    #[test]
    fn an_owned_write_outlives_the_borrow_and_keeps_its_place() {
        let lock = Arc::new(RwLock::new(0));
        let mut inside = lock.try_read().expect("free");
        // Made from a borrow that ends at once: the future owns the lock.
        let mut writer = Polled::new(Arc::clone(&lock).write_owned());
        writer.pending();
        for _ in 0..MAX_BYPASS {
            let next = lock.try_read().expect("a read barges past the writer");
            drop(inside);
            inside = next;
            writer.pending();
        }
        assert!(
            lock.try_read().is_none(),
            "the owned writer counted its bypasses like a borrowed one"
        );
        drop(inside);
        let mut guard = writer.ready();
        *guard += 1;
        assert!(lock.try_read().is_none(), "the guard holds the write");
        drop(guard);
        assert_eq!(*lock.try_read().expect("released"), 1);
    }

    #[test]
    fn a_dropped_owned_write_passes_a_handoff_on() {
        let lock = Arc::new(RwLock::new(()));
        let mut inside = lock.try_read().expect("free");
        let mut writer = Polled::new(lock.write_owned());
        writer.pending();
        for _ in 0..MAX_BYPASS {
            let next = lock.try_read().expect("barge");
            drop(inside);
            inside = next;
            writer.pending();
        }
        let mut reader = Polled::new(lock.read());
        reader.pending();
        drop(inside);
        drop(writer);
        assert_eq!(reader.wakes(), 1, "the dropped handoff reached the reader");
        drop(reader.ready());
        assert!(lock.try_write().is_some());
    }

    #[test]
    fn a_withdrawn_writer_leaves_nothing_behind() {
        let lock = RwLock::new(());
        let r0 = lock.try_read().expect("free");
        Polled::new(lock.write()).pending();
        drop(r0);
        assert!(lock.try_write().is_some());
    }

    #[test]
    fn a_writer_handed_the_lock_but_dropped_passes_it_on() {
        let lock = RwLock::new(());
        let mut inside = lock.try_read().expect("free");
        let mut writer = Polled::new(lock.write());
        writer.pending();
        for _ in 0..MAX_BYPASS {
            let next = lock.try_read().expect("barge");
            drop(inside);
            inside = next;
            writer.pending();
        }
        let mut reader = Polled::new(lock.read());
        reader.pending();
        drop(inside);
        drop(writer);
        assert_eq!(reader.wakes(), 1, "the dropped handoff reached the reader");
        drop(reader.ready());
        assert!(lock.try_write().is_some());
    }

    #[test]
    fn one_upgradable_read_shares_with_readers_and_excludes_writers() {
        let lock = RwLock::new(1);
        let up = lock.try_upgradable_read().expect("free");
        let read = lock.try_read().expect("plain readers share with it");
        assert!(lock.try_upgradable_read().is_none(), "one at a time");
        assert!(lock.try_write().is_none());
        let up = up.try_upgrade().expect_err("a reader is inside");
        let mut upgrade = Polled::new(up.upgrade());
        upgrade.pending();
        assert!(lock.try_read().is_none(), "new readers are held back");
        drop(read);
        assert_eq!(upgrade.wakes(), 1);
        let mut write = upgrade.ready();
        *write += 1;
        drop(write);
        assert_eq!(*lock.try_read().expect("free"), 2);
    }

    #[test]
    fn a_dropped_upgrade_releases_the_upgradable_read() {
        let lock = RwLock::new(());
        let up = lock.try_upgradable_read().expect("free");
        let read = lock.try_read().expect("share");
        Polled::new(up.upgrade()).pending();
        assert!(lock.try_read().is_some(), "readers are welcome again");
        drop(read);
        assert!(lock.try_write().is_some(), "nothing is left held");
    }

    #[test]
    fn threads_see_readers_xor_one_writer() {
        let lock = Arc::new(RwLock::new(()));
        let inside = Arc::new(AtomicIsize::new(0));
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let (lock, inside) = (Arc::clone(&lock), Arc::clone(&inside));
                std::thread::spawn(move || {
                    for i in 0..ITERATIONS {
                        match (i + t) % 4 {
                            0 => {
                                let _w = block_on(lock.write());
                                assert_eq!(inside.swap(-1, StdOrdering::SeqCst), 0);
                                std::hint::spin_loop();
                                assert_eq!(inside.swap(0, StdOrdering::SeqCst), -1);
                            }
                            1 => {
                                let up = block_on(lock.upgradable_read());
                                assert!(inside.load(StdOrdering::SeqCst) >= 0);
                                let _w = block_on(up.upgrade());
                                assert_eq!(inside.swap(-1, StdOrdering::SeqCst), 0);
                                assert_eq!(inside.swap(0, StdOrdering::SeqCst), -1);
                            }
                            _ => {
                                let _r = block_on(lock.read());
                                assert!(inside.fetch_add(1, StdOrdering::SeqCst) >= 0);
                                std::hint::spin_loop();
                                inside.fetch_sub(1, StdOrdering::SeqCst);
                            }
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("worker");
        }
        assert!(lock.try_write().is_some());
    }
}
