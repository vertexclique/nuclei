//! A fair asynchronous mutex: a one-permit semaphore around a value.

#![allow(unsafe_code)]

use core::cell::UnsafeCell;
use core::error::Error;
use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr;
use core::task::{Context, Poll};
use std::sync::Arc;

use super::internal;
use super::raw_semaphore::{PermitWait, RawSemaphore};

/// Error returned from [`Mutex::try_lock`], [`RwLock::try_read`] and
/// [`RwLock::try_write`] and their `_owned` forms.
///
/// `Mutex::try_lock` fails when the mutex is locked, or is owed to a waiter
/// that was passed over too often. `RwLock::try_read` fails when a writer
/// holds the lock, an upgrade is under way, or a waiter is owed the lock.
/// `RwLock::try_write` fails when any reader or writer holds it, or a waiter
/// is owed it.
///
/// [`RwLock::try_read`]: super::RwLock::try_read
/// [`RwLock::try_write`]: super::RwLock::try_write
#[derive(Debug)]
pub struct TryLockError(pub(crate) ());

impl fmt::Display for TryLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation would block")
    }
}

impl Error for TryLockError {}

/// A mutual-exclusion lock whose wait is a future.
///
/// Uncontended, [`try_lock`](Self::try_lock) and a guard's drop are one
/// atomic read-modify-write each and allocate nothing. A caller takes the
/// lock whenever it is free, even while others wait; under contention
/// [`lock`](Self::lock) spins briefly within the poll (see the
/// [module docs](super#what-every-primitive-guarantees)), then queues and
/// returns `Pending`.
/// An unlock wakes the oldest waiter to try again; a waiter that loses
/// [`MAX_BYPASS`](super::MAX_BYPASS) times is handed the lock directly,
/// and `try_lock` fails until it has had it.
///
/// It does not poison: a panic in a holder unlocks it.
///
/// ```
/// use nuclei::sync::Mutex;
///
/// let counter = Mutex::new(0u32);
/// *counter.try_lock().expect("uncontended") += 1;
/// assert_eq!(counter.into_inner(), 1);
/// ```
pub struct Mutex<T: ?Sized> {
    sem: RawSemaphore,
    value: UnsafeCell<T>,
}

// SAFETY: the value moves with the mutex.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
// SAFETY: the semaphore admits one guard at a time, and a guard hands out
// the value only as `&mut T` or `&T` under that exclusion.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    loom_const_fn! {
        /// An unlocked mutex holding `value`.
        pub fn new(value: T) -> Self {
            Self { sem: RawSemaphore::new(1), value: UnsafeCell::new(value) }
        }
    }

    loom_const_fn! {
        /// An unlocked mutex holding `value`, usable in a `static`.
        pub fn const_new(value: T) -> Self {
            Self { sem: RawSemaphore::new(1), value: UnsafeCell::new(value) }
        }
    }

    loom_const_fn! {
        /// An unlocked mutex holding `value` whose contended acquires never
        /// spin: they queue at once. For a mutex its holders keep across
        /// long waits of their own (I/O, say), where a spin could only burn
        /// the caller's time.
        #[cfg(test)]
        pub(crate) fn new_without_spin(value: T) -> Self {
            Self { sem: RawSemaphore::without_spin(1), value: UnsafeCell::new(value) }
        }
    }

    /// Consumes the mutex and returns its value.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

#[cfg(all(test, not(loom)))]
impl<T: ?Sized> Mutex<T> {
    /// The rounds a contended acquire of this mutex would spin now.
    pub(crate) fn spin_rounds(&self) -> u32 {
        use super::queue::Policy;
        self.sem.queue().spin.rounds()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Takes the permit if it is free and no handoff is owed.
    #[inline]
    fn try_acquire(&self) -> bool {
        match self.sem.take_one() {
            Ok(()) => true,
            Err(seen) => self.sem.try_acquire_one_from(seen),
        }
    }

    /// Locks the mutex if it is free and no handoff is owed.
    ///
    /// # Errors
    ///
    /// [`TryLockError`] when the mutex is locked or owed to a waiter.
    #[inline]
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError> {
        if self.try_acquire() {
            Ok(MutexGuard::new(self))
        } else {
            Err(TryLockError(()))
        }
    }

    /// [`try_lock`](Self::try_lock) through an `Arc`, so the guard can
    /// outlive the borrow.
    ///
    /// # Errors
    ///
    /// [`TryLockError`] when the mutex is locked or owed to a waiter.
    pub fn try_lock_owned(self: Arc<Self>) -> Result<OwnedMutexGuard<T>, TryLockError> {
        if self.try_acquire() {
            Ok(OwnedMutexGuard {
                mutex: self,
                _value: PhantomData,
            })
        } else {
            Err(TryLockError(()))
        }
    }

    /// Waits for the lock. Uncontended, the first poll is one load and one
    /// atomic read-modify-write. Contended, a poll may spin for up to the
    /// lock's spin budget before it queues, and again after a retry it lost,
    /// before it returns `Pending`; it is lock-free, not wait-free: see
    /// [Progress](super#progress).
    ///
    /// Dropping the future withdraws the wait, and passes on a lock handed
    /// to it meanwhile.
    #[inline]
    pub fn lock(&self) -> Lock<'_, T> {
        Lock {
            mutex: self,
            wait: PermitWait::new(),
        }
    }

    /// [`lock`](Self::lock) through an `Arc`, so the guard can outlive the
    /// borrow.
    pub fn lock_owned(self: Arc<Self>) -> LockOwned<T> {
        LockOwned {
            mutex: self,
            wait: PermitWait::new(),
        }
    }

    /// [`lock`](Self::lock) for a thread that may block: it parks the
    /// calling thread until the lock is free. On a nuclei shard (inside one of its tasks) it does not park the thread: it runs a nested `block_on`, so the shard keeps running its other tasks meanwhile.
    /// Inside another runtime's task it blocks that task's executor thread,
    /// so do not: the task that would unlock may be the one that thread was
    /// running.
    ///
    /// # Panics
    ///
    /// On `wasm32`, where no other thread can unlock, when the lock is held:
    /// the call could never end.
    pub fn blocking_lock(&self) -> MutexGuard<'_, T> {
        internal::blocking(self.lock())
    }

    /// [`blocking_lock`](Self::blocking_lock) through an `Arc`, so the guard
    /// can outlive the borrow.
    ///
    /// # Panics
    ///
    /// As for [`blocking_lock`](Self::blocking_lock).
    pub fn blocking_lock_owned(self: Arc<Self>) -> OwnedMutexGuard<T> {
        internal::blocking(self.lock_owned())
    }

    /// The value, borrowed mutably; `&mut self` proves no guard exists.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

impl<T> From<T> for Mutex<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Mutex");
        match self.try_lock() {
            Ok(inner) => d.field("data", &&*inner),
            Err(_) => d.field("data", &format_args!("<locked>")),
        };
        d.finish()
    }
}

/// Exclusive access to a [`Mutex`]'s value; unlocks on drop.
///
/// The release is one atomic read-modify-write when no waiter needs telling.
/// Otherwise it is lock-free, not wait-free: it returns after one pass over
/// the queue, plus at most one for each operation of another thread that
/// lands on the queue meanwhile (see [Progress](super#progress)).
#[must_use = "dropping the guard unlocks the mutex at once"]
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
    // `Sync` only when `T: Sync`, since `&MutexGuard` reaches `&T`.
    _value: PhantomData<&'a mut T>,
}

impl<'a, T: ?Sized> MutexGuard<'a, T> {
    fn new(mutex: &'a Mutex<T>) -> Self {
        Self {
            mutex,
            _value: PhantomData,
        }
    }

    /// The mutex, once the guard is spent: unlocking is left to whoever
    /// takes it over.
    fn into_mutex(self) -> &'a Mutex<T> {
        ManuallyDrop::new(self).mutex
    }

    /// A guard for a component of the locked data. It cannot fail: this
    /// guard already holds the lock.
    ///
    /// An associated function, `MutexGuard::map(guard, f)`: a method would
    /// shadow methods of the locked data.
    #[inline]
    pub fn map<U, F>(mut this: Self, f: F) -> MappedMutexGuard<'a, U>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> &mut U,
    {
        let data = ptr::from_mut(f(&mut *this));
        let mutex = this.into_mutex();
        MappedMutexGuard {
            sem: &mutex.sem,
            data,
            _value: PhantomData,
        }
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the lock still held.
    #[inline]
    pub fn try_map<U, F>(mut this: Self, f: F) -> Result<MappedMutexGuard<'a, U>, Self>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        let data = match f(&mut *this) {
            Some(data) => ptr::from_mut(data),
            None => return Err(this),
        };
        let mutex = this.into_mutex();
        Ok(MappedMutexGuard {
            sem: &mutex.sem,
            data,
            _value: PhantomData,
        })
    }

    /// The mutex this guard locks, for a caller that wants to unlock and
    /// lock again.
    #[inline]
    pub fn mutex(this: &Self) -> &'a Mutex<T> {
        this.mutex
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard holds the mutex's only permit.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.sem.release(1);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// Exclusive access to a [`Mutex`] held through an `Arc`; unlocks on drop.
/// The release makes the progress [`MutexGuard`]'s does.
#[must_use = "dropping the guard unlocks the mutex at once"]
pub struct OwnedMutexGuard<T: ?Sized> {
    mutex: Arc<Mutex<T>>,
    // `Sync` only when `T: Sync`, since `&OwnedMutexGuard` reaches `&T`.
    _value: PhantomData<UnsafeCell<T>>,
}

// SAFETY: sharing the guard shares `&T`, which needs `T: Sync`.
unsafe impl<T: ?Sized + Send + Sync> Sync for OwnedMutexGuard<T> {}

impl<T: ?Sized> OwnedMutexGuard<T> {
    /// The `Arc` this guard keeps the mutex alive through, once the guard is
    /// spent: unlocking is left to whoever takes it over.
    fn into_arc(self) -> Arc<Mutex<T>> {
        let spent = ManuallyDrop::new(self);
        // SAFETY: `spent` is never dropped, so this is the only read of the
        // `Arc` and it is moved out exactly once.
        unsafe { ptr::read(&spent.mutex) }
    }

    /// A guard for a component of the locked data. It cannot fail: this
    /// guard already holds the lock.
    ///
    /// An associated function, `OwnedMutexGuard::map(guard, f)`: a method
    /// would shadow methods of the locked data.
    #[inline]
    pub fn map<U, F>(mut this: Self, f: F) -> OwnedMappedMutexGuard<T, U>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> &mut U,
    {
        let data = ptr::from_mut(f(&mut *this));
        OwnedMappedMutexGuard {
            mutex: this.into_arc(),
            data,
        }
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the lock still held.
    #[inline]
    pub fn try_map<U, F>(mut this: Self, f: F) -> Result<OwnedMappedMutexGuard<T, U>, Self>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        let data = match f(&mut *this) {
            Some(data) => ptr::from_mut(data),
            None => return Err(this),
        };
        Ok(OwnedMappedMutexGuard {
            mutex: this.into_arc(),
            data,
        })
    }

    /// The `Arc` of the mutex this guard locks.
    #[inline]
    pub fn mutex(this: &Self) -> &Arc<Mutex<T>> {
        &this.mutex
    }
}

impl<T: ?Sized> Deref for OwnedMutexGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: this guard holds the mutex's only permit.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> DerefMut for OwnedMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T: ?Sized> Drop for OwnedMutexGuard<T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.sem.release(1);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for OwnedMutexGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for OwnedMutexGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// A guard for a component of a [`Mutex`]'s value, made by
/// [`MutexGuard::map`]; unlocks on drop. The release makes the progress
/// [`MutexGuard`]'s does.
#[must_use = "dropping the guard unlocks the mutex at once"]
pub struct MappedMutexGuard<'a, T: ?Sized> {
    sem: &'a RawSemaphore,
    data: *mut T,
    // Tells the borrow checker this guard holds a `&mut T`.
    _value: PhantomData<&'a mut T>,
}

// SAFETY: the guard hands out `&T` and `&mut T` to the component only, as a
// `MutexGuard` does to the whole value, so it shares and moves like a `&T`
// and a `&mut T`.
unsafe impl<T: ?Sized + Sync> Sync for MappedMutexGuard<'_, T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Send for MappedMutexGuard<'_, T> {}

impl<'a, T: ?Sized> MappedMutexGuard<'a, T> {
    /// The semaphore, once the guard is spent: unlocking is left to whoever
    /// takes it over.
    fn into_sem(self) -> &'a RawSemaphore {
        ManuallyDrop::new(self).sem
    }

    /// A guard for a component of the component. It cannot fail: this guard
    /// already holds the lock.
    ///
    /// An associated function, `MappedMutexGuard::map(guard, f)`: a method
    /// would shadow methods of the locked data.
    #[inline]
    pub fn map<U, F>(mut this: Self, f: F) -> MappedMutexGuard<'a, U>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> &mut U,
    {
        let data = ptr::from_mut(f(&mut *this));
        MappedMutexGuard {
            sem: this.into_sem(),
            data,
            _value: PhantomData,
        }
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the lock still held.
    #[inline]
    pub fn try_map<U, F>(mut this: Self, f: F) -> Result<MappedMutexGuard<'a, U>, Self>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        let data = match f(&mut *this) {
            Some(data) => ptr::from_mut(data),
            None => return Err(this),
        };
        Ok(MappedMutexGuard {
            sem: this.into_sem(),
            data,
            _value: PhantomData,
        })
    }
}

impl<T: ?Sized> Deref for MappedMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: `data` points into the value of the mutex whose permit
        // this guard holds.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> DerefMut for MappedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.data }
    }
}

impl<T: ?Sized> Drop for MappedMutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.sem.release(1);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for MappedMutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for MappedMutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// A guard for a component `U` of the value of a [`Mutex<T>`] held through
/// an `Arc`, made by [`OwnedMutexGuard::map`]; unlocks on drop. The release
/// makes the progress [`MutexGuard`]'s does.
#[must_use = "dropping the guard unlocks the mutex at once"]
pub struct OwnedMappedMutexGuard<T: ?Sized, U: ?Sized = T> {
    mutex: Arc<Mutex<T>>,
    data: *mut U,
}

// SAFETY: the guard hands out `&U` and `&mut U` into a value of the mutex it
// holds the lock of, and carries the `Arc` that keeps the mutex alive.
unsafe impl<T: ?Sized + Send + Sync, U: ?Sized + Send + Sync> Sync for OwnedMappedMutexGuard<T, U> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send, U: ?Sized + Send> Send for OwnedMappedMutexGuard<T, U> {}

impl<T: ?Sized, U: ?Sized> OwnedMappedMutexGuard<T, U> {
    /// The `Arc`, once the guard is spent: unlocking is left to whoever
    /// takes it over.
    fn into_arc(self) -> Arc<Mutex<T>> {
        let spent = ManuallyDrop::new(self);
        // SAFETY: `spent` is never dropped, so this is the only read of the
        // `Arc` and it is moved out exactly once.
        unsafe { ptr::read(&spent.mutex) }
    }

    /// A guard for a component of the component. It cannot fail: this guard
    /// already holds the lock.
    ///
    /// An associated function, `OwnedMappedMutexGuard::map(guard, f)`: a
    /// method would shadow methods of the locked data.
    #[inline]
    pub fn map<S, F>(mut this: Self, f: F) -> OwnedMappedMutexGuard<T, S>
    where
        S: ?Sized,
        F: FnOnce(&mut U) -> &mut S,
    {
        let data = ptr::from_mut(f(&mut *this));
        OwnedMappedMutexGuard {
            mutex: this.into_arc(),
            data,
        }
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the lock still held.
    #[inline]
    pub fn try_map<S, F>(mut this: Self, f: F) -> Result<OwnedMappedMutexGuard<T, S>, Self>
    where
        S: ?Sized,
        F: FnOnce(&mut U) -> Option<&mut S>,
    {
        let data = match f(&mut *this) {
            Some(data) => ptr::from_mut(data),
            None => return Err(this),
        };
        Ok(OwnedMappedMutexGuard {
            mutex: this.into_arc(),
            data,
        })
    }
}

impl<T: ?Sized, U: ?Sized> Deref for OwnedMappedMutexGuard<T, U> {
    type Target = U;

    fn deref(&self) -> &U {
        // SAFETY: `data` points into the value of the mutex whose permit
        // this guard holds.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized, U: ?Sized> DerefMut for OwnedMappedMutexGuard<T, U> {
    fn deref_mut(&mut self) -> &mut U {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.data }
    }
}

impl<T: ?Sized, U: ?Sized> Drop for OwnedMappedMutexGuard<T, U> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.sem.release(1);
    }
}

impl<T: ?Sized, U: ?Sized + fmt::Debug> fmt::Debug for OwnedMappedMutexGuard<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized, U: ?Sized + fmt::Display> fmt::Display for OwnedMappedMutexGuard<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// The future [`Mutex::lock`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct Lock<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
    wait: PermitWait,
}

impl<'a, T: ?Sized> Future for Lock<'a, T> {
    type Output = MutexGuard<'a, T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<MutexGuard<'a, T>> {
        let this = self.get_mut();
        this.wait
            .poll_one(&this.mutex.sem, cx.waker())
            .map(|()| MutexGuard::new(this.mutex))
    }
}

impl<T: ?Sized> Drop for Lock<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.wait.cancel(&self.mutex.sem, 1);
    }
}

impl<T: ?Sized> fmt::Debug for Lock<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lock").finish_non_exhaustive()
    }
}

/// The future [`Mutex::lock_owned`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct LockOwned<T: ?Sized> {
    mutex: Arc<Mutex<T>>,
    wait: PermitWait,
}

impl<T: ?Sized> Future for LockOwned<T> {
    type Output = OwnedMutexGuard<T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<OwnedMutexGuard<T>> {
        let this = self.get_mut();
        this.wait
            .poll_one(&this.mutex.sem, cx.waker())
            .map(|()| OwnedMutexGuard {
                mutex: Arc::clone(&this.mutex),
                _value: PhantomData,
            })
    }
}

impl<T: ?Sized> Drop for LockOwned<T> {
    #[inline]
    fn drop(&mut self) {
        self.wait.cancel(&self.mutex.sem, 1);
    }
}

impl<T: ?Sized> fmt::Debug for LockOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LockOwned").finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::{Polled, block_on};

    const ITERATIONS: usize = if cfg!(miri) { 20 } else { 5_000 };

    /// A mutex built without spin queues its contended callers at once:
    /// its budget is zero from the start and stays zero through a contended
    /// wait, its wake and its retry, where a spinning mutex's starts above
    /// zero.
    #[test]
    fn a_mutex_built_without_spin_never_spins() {
        assert!(Mutex::new(()).spin_rounds() > 0, "a plain mutex spins");
        let mutex = Mutex::new_without_spin(0u8);
        assert_eq!(mutex.spin_rounds(), 0);
        let held = mutex.try_lock().expect("free");
        let mut waiter = Polled::new(mutex.lock());
        waiter.pending();
        drop(held);
        assert_eq!(waiter.wakes(), 1);
        *waiter.ready() += 1;
        assert_eq!(mutex.spin_rounds(), 0, "the mutex never spins");
    }

    #[test]
    fn releases_wake_waiters_in_arrival_order() {
        let mutex = Mutex::new(Vec::new());
        let held = mutex.try_lock().expect("free");
        let mut waiters: Vec<_> = (0..3).map(|_| Polled::new(mutex.lock())).collect();
        for waiter in &mut waiters {
            waiter.pending();
        }
        drop(held);
        for (i, waiter) in waiters.iter_mut().enumerate() {
            assert_eq!(
                waiter.wakes(),
                1,
                "waiter {i} is woken by the release before it"
            );
            let mut guard = waiter.ready();
            guard.push(i);
            assert!(mutex.try_lock().is_err(), "the woken waiter holds it");
        }
        assert_eq!(*mutex.try_lock().expect("free"), vec![0, 1, 2]);
    }

    #[test]
    fn a_waiter_that_never_saw_its_grant_passes_the_lock_on() {
        let mutex = Mutex::new(());
        let held = mutex.try_lock().expect("free");
        let mut first = Polled::new(mutex.lock());
        first.pending();
        let mut second = Polled::new(mutex.lock());
        second.pending();
        drop(held);
        drop(first);
        drop(second.ready());
        assert!(mutex.try_lock().is_ok());
    }

    #[test]
    fn a_waker_changed_between_polls_is_the_one_woken() {
        let mutex = Mutex::new(());
        let held = mutex.try_lock().expect("free");
        let mut lock = core::pin::pin!(mutex.lock());
        let first = Polled::new(core::future::pending::<()>());
        let second = Polled::new(core::future::pending::<()>());
        let poll_with = |lock: &mut Pin<&mut Lock<'_, ()>>, waker: &core::task::Waker| {
            lock.as_mut()
                .poll(&mut Context::from_waker(waker))
                .is_pending()
        };
        let (first_waker, second_waker) = (first.waker(), second.waker());
        assert!(poll_with(&mut lock, &first_waker));
        assert!(poll_with(&mut lock, &second_waker));
        drop(held);
        assert_eq!((first.wakes(), second.wakes()), (0, 1));
        assert!(!poll_with(&mut lock, &second_waker));
    }

    #[test]
    fn owned_guards_and_get_mut() {
        let mut mutex = Mutex::new(1);
        *mutex.get_mut() += 1;
        let mutex = Arc::new(mutex);
        let guard = block_on(Arc::clone(&mutex).lock_owned());
        let mut next = Polled::new(Arc::clone(&mutex).lock_owned());
        next.pending();
        assert_eq!(*guard, 2);
        drop(guard);
        assert_eq!(*next.ready(), 2);
    }

    #[test]
    fn a_failed_try_lock_reports_it_would_block() {
        let mutex = Arc::new(Mutex::new(0));
        let held = Arc::clone(&mutex).try_lock_owned().expect("free");
        let err = mutex.try_lock().unwrap_err();
        assert_eq!(err.to_string(), "operation would block");
        assert_eq!(format!("{err:?}"), "TryLockError(())");
        assert!(Arc::clone(&mutex).try_lock_owned().is_err());
        drop(held);
        assert!(mutex.try_lock().is_ok());
    }

    #[test]
    fn a_mapped_guard_keeps_the_lock_until_it_drops() {
        let mutex = Arc::new(Mutex::new((1, String::from("a"))));
        let mut number = MutexGuard::map(block_on(mutex.lock()), |pair| &mut pair.0);
        *number += 1;
        assert!(mutex.try_lock().is_err(), "the mapped guard holds the lock");
        let mut waiter = Polled::new(mutex.lock());
        waiter.pending();
        drop(number);
        assert_eq!(waiter.wakes(), 1);
        assert_eq!(waiter.ready().0, 2);

        let owned = OwnedMutexGuard::map(block_on(Arc::clone(&mutex).lock_owned()), |pair| {
            &mut pair.1
        });
        let mut tail = OwnedMappedMutexGuard::map(owned, |text| text.as_mut_str());
        tail.make_ascii_uppercase();
        assert_eq!(&*tail, "A");
        assert!(mutex.try_lock().is_err());
        drop(tail);
        assert_eq!(mutex.try_lock().expect("unlocked").1, "A");
    }

    #[test]
    fn try_map_gives_the_guard_back_when_nothing_is_found() {
        let mutex = Mutex::new(Some(5));
        let guard = mutex.try_lock().expect("free");
        let guard = MutexGuard::try_map(guard, |value| value.as_mut().filter(|n| **n > 9))
            .expect_err("nothing to map");
        assert!(mutex.try_lock().is_err(), "the lock is still held");
        let mut mapped =
            MutexGuard::try_map(guard, |value| value.as_mut()).expect("a five is there");
        *mapped += 1;
        let mapped = MappedMutexGuard::try_map(mapped, |n| (*n > 100).then_some(n))
            .expect_err("six is small");
        let mut mapped = MappedMutexGuard::map(mapped, |n| n);
        *mapped += 1;
        drop(mapped);
        assert_eq!(mutex.into_inner(), Some(7));
    }

    #[test]
    fn a_guard_names_its_mutex() {
        let mutex = Arc::new(Mutex::new(3));
        let guard = block_on(mutex.lock());
        let same = MutexGuard::mutex(&guard);
        assert!(core::ptr::eq(same, &*mutex));
        drop(guard);
        let owned = block_on(Arc::clone(&mutex).lock_owned());
        assert!(Arc::ptr_eq(OwnedMutexGuard::mutex(&owned), &mutex));
        assert_eq!(format!("{owned} {owned:?}"), "3 3");
    }

    #[test]
    fn blocking_locks_wait_for_the_holder_on_another_thread() {
        let mutex = Arc::new(Mutex::new(0));
        let held = mutex.blocking_lock();
        let waiter = {
            let mutex = Arc::clone(&mutex);
            std::thread::spawn(move || {
                *mutex.blocking_lock() += 1;
                *Arc::clone(&mutex).blocking_lock_owned() += 1;
            })
        };
        drop(held);
        waiter.join().expect("waiter");
        assert_eq!(*mutex.try_lock().expect("free"), 2);
    }

    #[test]
    fn a_const_mutex_and_conversions() {
        static COUNTER: Mutex<u32> = Mutex::const_new(5);
        *COUNTER.try_lock().expect("free") += 1;
        assert_eq!(*COUNTER.try_lock().expect("free"), 6);
        let from: Mutex<u8> = 9.into();
        assert_eq!(from.into_inner(), 9);
        assert_eq!(Mutex::<u8>::default().into_inner(), 0);
        let mutex = Mutex::new(vec![1]);
        assert_eq!(format!("{mutex:?}"), "Mutex { data: [1] }");
        let held = mutex.try_lock().expect("free");
        assert_eq!(format!("{mutex:?}"), "Mutex { data: <locked> }");
        drop(held);
    }

    #[test]
    fn threads_never_overlap_inside_the_lock() {
        let mutex = Arc::new(Mutex::new(0usize));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                std::thread::spawn(move || {
                    for _ in 0..ITERATIONS {
                        let mut guard = block_on(mutex.lock());
                        let seen = *guard;
                        std::hint::spin_loop();
                        *guard = seen + 1;
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("worker");
        }
        assert_eq!(*mutex.try_lock().expect("free"), 4 * ITERATIONS);
    }
}
