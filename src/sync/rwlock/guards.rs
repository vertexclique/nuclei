//! The guards of a [`RwLock`]: the read and write guards, the guards that map
//! to a component of the value, and their forms that keep the lock alive
//! through an `Arc`.
//!
//! Every guard holds a pointer to the value (or to the component it was
//! mapped to) next to the lock it releases, so mapping and downgrading never
//! touch the lock: they only change which guard holds the release.

#![allow(unsafe_code)]

use core::fmt;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::ptr;
use std::sync::Arc;

use super::RwLock;
use crate::sync::raw_rwlock::RawRwLock;

/// Shared access to a [`RwLock`]'s value, or to a component of it; releases
/// on drop.
///
/// The release is one atomic read-modify-write when no waiter needs telling.
/// Otherwise it is lock-free, not wait-free: it returns after one pass over
/// the queue, plus at most one for each operation of another thread that
/// lands on the queue meanwhile (see [Progress](crate::sync#progress)).
#[must_use = "dropping the guard releases the lock at once"]
pub struct RwLockReadGuard<'a, T: ?Sized> {
    raw: &'a RawRwLock,
    data: *const T,
    marker: PhantomData<&'a T>,
}

// SAFETY: the guard is a shared reference to the component, and a read
// excludes every writer for as long as it lives.
unsafe impl<T: ?Sized + Sync> Send for RwLockReadGuard<'_, T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLockReadGuard<'_, T> {}

impl<'a, T: ?Sized> RwLockReadGuard<'a, T> {
    pub(super) fn new(lock: &'a RwLock<T>) -> Self {
        Self {
            raw: &lock.raw,
            data: lock.value.get(),
            marker: PhantomData,
        }
    }

    /// The lock and component, once the guard is spent: releasing is left
    /// to whoever takes it over.
    fn into_parts(self) -> (&'a RawRwLock, *const T) {
        let spent = ManuallyDrop::new(self);
        (spent.raw, spent.data)
    }

    /// A guard for a component of the locked data. It cannot fail: this
    /// guard already holds a read.
    ///
    /// An associated function, `RwLockReadGuard::map(guard, f)`: a method
    /// would shadow methods of the locked data.
    #[inline]
    pub fn map<F, U>(this: Self, f: F) -> RwLockReadGuard<'a, U>
    where
        U: ?Sized,
        F: FnOnce(&T) -> &U,
    {
        let data = ptr::from_ref(f(&*this));
        let (raw, _) = this.into_parts();
        RwLockReadGuard {
            raw,
            data,
            marker: PhantomData,
        }
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the read still held.
    #[inline]
    pub fn try_map<F, U>(this: Self, f: F) -> Result<RwLockReadGuard<'a, U>, Self>
    where
        U: ?Sized,
        F: FnOnce(&T) -> Option<&U>,
    {
        let data = match f(&*this) {
            Some(data) => ptr::from_ref(data),
            None => return Err(this),
        };
        let (raw, _) = this.into_parts();
        Ok(RwLockReadGuard {
            raw,
            data,
            marker: PhantomData,
        })
    }
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: a read excludes every writer, and `data` points into the
        // value of the lock it holds.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.raw.read_unlock();
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// Shared access to a [`RwLock`] held through an `Arc`, to its value or to
/// a component `U` of it; releases on drop. The release makes the progress
/// [`RwLockReadGuard`]'s does.
#[must_use = "dropping the guard releases the lock at once"]
pub struct OwnedRwLockReadGuard<T: ?Sized, U: ?Sized = T> {
    lock: Arc<RwLock<T>>,
    data: *const U,
    _p: PhantomData<T>,
}

// SAFETY: the guard shares `&U`, and the lock it keeps alive can drop its
// `T` on whichever thread drops the last `Arc`.
unsafe impl<T: ?Sized + Send + Sync, U: ?Sized + Sync> Send for OwnedRwLockReadGuard<T, U> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send + Sync, U: ?Sized + Send + Sync> Sync for OwnedRwLockReadGuard<T, U> {}

impl<T: ?Sized> OwnedRwLockReadGuard<T> {
    pub(super) fn new(lock: Arc<RwLock<T>>) -> Self {
        Self {
            data: lock.value.get(),
            lock,
            _p: PhantomData,
        }
    }
}

impl<T: ?Sized, U: ?Sized> OwnedRwLockReadGuard<T, U> {
    /// The `Arc` and component, once the guard is spent: releasing is left
    /// to whoever takes it over.
    fn into_parts(self) -> (Arc<RwLock<T>>, *const U) {
        let spent = ManuallyDrop::new(self);
        // SAFETY: `spent` is never dropped, so this is the only read of the
        // `Arc` and it is moved out exactly once.
        (unsafe { ptr::read(&spent.lock) }, spent.data)
    }

    /// A guard for a component of the locked data. It cannot fail: this
    /// guard already holds a read.
    ///
    /// An associated function, `OwnedRwLockReadGuard::map(guard, f)`: a
    /// method would shadow methods of the locked data.
    #[inline]
    pub fn map<F, V>(this: Self, f: F) -> OwnedRwLockReadGuard<T, V>
    where
        V: ?Sized,
        F: FnOnce(&U) -> &V,
    {
        let data = ptr::from_ref(f(&*this));
        let (lock, _) = this.into_parts();
        OwnedRwLockReadGuard {
            lock,
            data,
            _p: PhantomData,
        }
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the read still held.
    #[inline]
    pub fn try_map<F, V>(this: Self, f: F) -> Result<OwnedRwLockReadGuard<T, V>, Self>
    where
        V: ?Sized,
        F: FnOnce(&U) -> Option<&V>,
    {
        let data = match f(&*this) {
            Some(data) => ptr::from_ref(data),
            None => return Err(this),
        };
        let (lock, _) = this.into_parts();
        Ok(OwnedRwLockReadGuard {
            lock,
            data,
            _p: PhantomData,
        })
    }

    /// The `Arc` of the lock this guard holds a read of.
    #[inline]
    pub fn rwlock(this: &Self) -> &Arc<RwLock<T>> {
        &this.lock
    }
}

impl<T: ?Sized, U: ?Sized> Deref for OwnedRwLockReadGuard<T, U> {
    type Target = U;

    fn deref(&self) -> &U {
        // SAFETY: a read excludes every writer, and `data` points into the
        // value of the lock it holds.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized, U: ?Sized> Drop for OwnedRwLockReadGuard<T, U> {
    fn drop(&mut self) {
        self.lock.raw.read_unlock();
    }
}

impl<T: ?Sized, U: ?Sized + fmt::Debug> fmt::Debug for OwnedRwLockReadGuard<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized, U: ?Sized + fmt::Display> fmt::Display for OwnedRwLockReadGuard<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// Exclusive access to a [`RwLock`]'s value; releases on drop. The release
/// makes the progress [`RwLockReadGuard`]'s does.
#[must_use = "dropping the guard releases the lock at once"]
pub struct RwLockWriteGuard<'a, T: ?Sized> {
    raw: &'a RawRwLock,
    data: *mut T,
    marker: PhantomData<&'a mut T>,
}

// SAFETY: the guard is a unique reference to the value, and the write
// excludes every other guard for as long as it lives.
unsafe impl<T: ?Sized + Send + Sync> Send for RwLockWriteGuard<'_, T> {}
// SAFETY: as above, shared as a `&T`.
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLockWriteGuard<'_, T> {}

impl<'a, T: ?Sized> RwLockWriteGuard<'a, T> {
    pub(super) fn new(lock: &'a RwLock<T>) -> Self {
        Self {
            raw: &lock.raw,
            data: lock.value.get(),
            marker: PhantomData,
        }
    }

    /// The lock and value, once the guard is spent: releasing is left to
    /// whoever takes it over.
    fn into_parts(self) -> (&'a RawRwLock, *mut T) {
        let spent = ManuallyDrop::new(self);
        (spent.raw, spent.data)
    }

    /// A guard for a component of the locked data. It cannot fail: this
    /// guard already holds the write.
    ///
    /// An associated function, `RwLockWriteGuard::map(guard, f)`: a method
    /// would shadow methods of the locked data.
    #[inline]
    pub fn map<F, U>(mut this: Self, f: F) -> RwLockMappedWriteGuard<'a, U>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> &mut U,
    {
        let data = ptr::from_mut(f(&mut *this));
        let (raw, _) = this.into_parts();
        RwLockMappedWriteGuard {
            raw,
            data,
            marker: PhantomData,
        }
    }

    /// A read guard for a component of the locked data, made while the
    /// write is turned into a read in one step, as
    /// [`downgrade`](Self::downgrade) does. Inside `f` the write is still
    /// held, yet `f` is given only a `&T`: a `&mut T` could be kept past the
    /// downgrade through interior mutability.
    #[inline]
    pub fn downgrade_map<F, U>(this: Self, f: F) -> RwLockReadGuard<'a, U>
    where
        U: ?Sized,
        F: FnOnce(&T) -> &U,
    {
        let data = ptr::from_ref(f(&*this));
        let (raw, _) = this.into_parts();
        // The read guard exists before the step, so a panic out of it still
        // releases the read the step made.
        let guard = RwLockReadGuard {
            raw,
            data,
            marker: PhantomData,
        };
        raw.downgrade();
        guard
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the write still held.
    #[inline]
    pub fn try_map<F, U>(mut this: Self, f: F) -> Result<RwLockMappedWriteGuard<'a, U>, Self>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        let data = match f(&mut *this) {
            Some(data) => ptr::from_mut(data),
            None => return Err(this),
        };
        let (raw, _) = this.into_parts();
        Ok(RwLockMappedWriteGuard {
            raw,
            data,
            marker: PhantomData,
        })
    }

    /// [`downgrade_map`](Self::downgrade_map) that gives the guard back when
    /// `f` finds no component: the lock is then neither released nor
    /// downgraded.
    #[inline]
    pub fn try_downgrade_map<F, U>(this: Self, f: F) -> Result<RwLockReadGuard<'a, U>, Self>
    where
        U: ?Sized,
        F: FnOnce(&T) -> Option<&U>,
    {
        let data = match f(&*this) {
            Some(data) => ptr::from_ref(data),
            None => return Err(this),
        };
        let (raw, _) = this.into_parts();
        let guard = RwLockReadGuard {
            raw,
            data,
            marker: PhantomData,
        };
        raw.downgrade();
        Ok(guard)
    }

    /// This guard as a [`RwLockMappedWriteGuard`], for a struct field that
    /// holds one. The same as `RwLockWriteGuard::map(guard, |me| me)`.
    #[inline]
    pub fn into_mapped(this: Self) -> RwLockMappedWriteGuard<'a, T> {
        Self::map(this, |me| me)
    }

    /// Turns the write into a read in one step, so no writer gets in
    /// between.
    ///
    /// This does not necessarily let more readers in: a writer may be next
    /// in line. The readers queued behind a writer-free front share the lock
    /// now, and are woken to do so.
    ///
    /// It makes the progress a release does (see [`RwLockReadGuard`]).
    ///
    /// ```
    /// use nuclei::sync::RwLock;
    ///
    /// let lock = RwLock::new(1);
    /// let mut write = lock.try_write().expect("free");
    /// *write += 1;
    /// let read = write.downgrade();
    /// assert_eq!(*read, 2);
    /// assert!(lock.try_write().is_err(), "the read is still held");
    /// assert!(lock.try_read().is_ok(), "readers share it");
    /// ```
    pub fn downgrade(self) -> RwLockReadGuard<'a, T> {
        let (raw, data) = self.into_parts();
        let guard = RwLockReadGuard {
            raw,
            data,
            marker: PhantomData,
        };
        raw.downgrade();
        guard
    }
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the write excludes every other guard.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.data }
    }
}

impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.raw.write_unlock();
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// Exclusive access to a component of a [`RwLock`]'s value, made by
/// [`RwLockWriteGuard::map`]; releases on drop. The release makes the
/// progress [`RwLockReadGuard`]'s does.
#[must_use = "dropping the guard releases the lock at once"]
pub struct RwLockMappedWriteGuard<'a, T: ?Sized> {
    raw: &'a RawRwLock,
    data: *mut T,
    marker: PhantomData<&'a mut T>,
}

// SAFETY: as for `RwLockWriteGuard`.
unsafe impl<T: ?Sized + Send + Sync> Send for RwLockMappedWriteGuard<'_, T> {}
// SAFETY: as for `RwLockWriteGuard`.
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLockMappedWriteGuard<'_, T> {}

impl<'a, T: ?Sized> RwLockMappedWriteGuard<'a, T> {
    /// The lock, once the guard is spent: releasing is left to whoever
    /// takes it over.
    fn into_raw(self) -> &'a RawRwLock {
        ManuallyDrop::new(self).raw
    }

    /// A guard for a component of the component. It cannot fail: this guard
    /// already holds the write.
    ///
    /// An associated function, `RwLockMappedWriteGuard::map(guard, f)`: a
    /// method would shadow methods of the locked data.
    #[inline]
    pub fn map<F, U>(mut this: Self, f: F) -> RwLockMappedWriteGuard<'a, U>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> &mut U,
    {
        let data = ptr::from_mut(f(&mut *this));
        RwLockMappedWriteGuard {
            raw: this.into_raw(),
            data,
            marker: PhantomData,
        }
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the write still held.
    #[inline]
    pub fn try_map<F, U>(mut this: Self, f: F) -> Result<RwLockMappedWriteGuard<'a, U>, Self>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        let data = match f(&mut *this) {
            Some(data) => ptr::from_mut(data),
            None => return Err(this),
        };
        Ok(RwLockMappedWriteGuard {
            raw: this.into_raw(),
            data,
            marker: PhantomData,
        })
    }
}

impl<T: ?Sized> Deref for RwLockMappedWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the write excludes every other guard, and `data` points
        // into the value of the lock it holds.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> DerefMut for RwLockMappedWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.data }
    }
}

impl<T: ?Sized> Drop for RwLockMappedWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.raw.write_unlock();
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLockMappedWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for RwLockMappedWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// Exclusive access to a [`RwLock`]'s value through an [`Arc`] the guard
/// holds; releases on drop. Made by [`RwLock::write_owned`]. The release
/// makes the progress [`RwLockReadGuard`]'s does.
#[must_use = "dropping the guard releases the lock at once"]
pub struct OwnedRwLockWriteGuard<T: ?Sized> {
    lock: Arc<RwLock<T>>,
    data: *mut T,
    _p: PhantomData<T>,
}

// SAFETY: the guard is a unique reference to the value, and the write
// excludes every other guard for as long as it lives.
unsafe impl<T: ?Sized + Send + Sync> Send for OwnedRwLockWriteGuard<T> {}
// SAFETY: as above, shared as a `&T`.
unsafe impl<T: ?Sized + Send + Sync> Sync for OwnedRwLockWriteGuard<T> {}

impl<T: ?Sized> OwnedRwLockWriteGuard<T> {
    pub(super) fn new(lock: Arc<RwLock<T>>) -> Self {
        Self {
            data: lock.value.get(),
            lock,
            _p: PhantomData,
        }
    }

    /// The `Arc` and value, once the guard is spent: releasing is left to
    /// whoever takes it over.
    fn into_parts(self) -> (Arc<RwLock<T>>, *mut T) {
        let spent = ManuallyDrop::new(self);
        // SAFETY: `spent` is never dropped, so this is the only read of the
        // `Arc` and it is moved out exactly once.
        (unsafe { ptr::read(&spent.lock) }, spent.data)
    }

    /// A guard for a component of the locked data. It cannot fail: this
    /// guard already holds the write.
    ///
    /// An associated function, `OwnedRwLockWriteGuard::map(guard, f)`: a
    /// method would shadow methods of the locked data.
    #[inline]
    pub fn map<F, U>(mut this: Self, f: F) -> OwnedRwLockMappedWriteGuard<T, U>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> &mut U,
    {
        let data = ptr::from_mut(f(&mut *this));
        let (lock, _) = this.into_parts();
        OwnedRwLockMappedWriteGuard {
            lock,
            data,
            _p: PhantomData,
        }
    }

    /// A read guard for a component of the locked data, made while the
    /// write is turned into a read in one step, as
    /// [`downgrade`](Self::downgrade) does. Inside `f` the write is still
    /// held, yet `f` is given only a `&T`: a `&mut T` could be kept past the
    /// downgrade through interior mutability.
    #[inline]
    pub fn downgrade_map<F, U>(this: Self, f: F) -> OwnedRwLockReadGuard<T, U>
    where
        U: ?Sized,
        F: FnOnce(&T) -> &U,
    {
        let data = ptr::from_ref(f(&*this));
        let (lock, _) = this.into_parts();
        let guard = OwnedRwLockReadGuard {
            lock,
            data,
            _p: PhantomData,
        };
        guard.lock.raw.downgrade();
        guard
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the write still held.
    #[inline]
    pub fn try_map<F, U>(mut this: Self, f: F) -> Result<OwnedRwLockMappedWriteGuard<T, U>, Self>
    where
        U: ?Sized,
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        let data = match f(&mut *this) {
            Some(data) => ptr::from_mut(data),
            None => return Err(this),
        };
        let (lock, _) = this.into_parts();
        Ok(OwnedRwLockMappedWriteGuard {
            lock,
            data,
            _p: PhantomData,
        })
    }

    /// [`downgrade_map`](Self::downgrade_map) that gives the guard back when
    /// `f` finds no component: the lock is then neither released nor
    /// downgraded.
    #[inline]
    pub fn try_downgrade_map<F, U>(this: Self, f: F) -> Result<OwnedRwLockReadGuard<T, U>, Self>
    where
        U: ?Sized,
        F: FnOnce(&T) -> Option<&U>,
    {
        let data = match f(&*this) {
            Some(data) => ptr::from_ref(data),
            None => return Err(this),
        };
        let (lock, _) = this.into_parts();
        let guard = OwnedRwLockReadGuard {
            lock,
            data,
            _p: PhantomData,
        };
        guard.lock.raw.downgrade();
        Ok(guard)
    }

    /// This guard as an [`OwnedRwLockMappedWriteGuard`], for a struct field
    /// that holds one. The same as `OwnedRwLockWriteGuard::map(guard, |me|
    /// me)`.
    #[inline]
    pub fn into_mapped(this: Self) -> OwnedRwLockMappedWriteGuard<T> {
        Self::map(this, |me| me)
    }

    /// Turns the write into a read in one step, so no writer gets in
    /// between. See [`RwLockWriteGuard::downgrade`].
    pub fn downgrade(self) -> OwnedRwLockReadGuard<T> {
        let (lock, data) = self.into_parts();
        let guard = OwnedRwLockReadGuard {
            lock,
            data,
            _p: PhantomData,
        };
        guard.lock.raw.downgrade();
        guard
    }

    /// The `Arc` of the lock this guard holds the write of.
    #[inline]
    pub fn rwlock(this: &Self) -> &Arc<RwLock<T>> {
        &this.lock
    }
}

impl<T: ?Sized> Deref for OwnedRwLockWriteGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the write excludes every other guard.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> DerefMut for OwnedRwLockWriteGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.data }
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

impl<T: ?Sized + fmt::Display> fmt::Display for OwnedRwLockWriteGuard<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

/// Exclusive access to a component `U` of the value of a [`RwLock<T>`] held
/// through an `Arc`, made by [`OwnedRwLockWriteGuard::map`]; releases on
/// drop. The release makes the progress [`RwLockReadGuard`]'s does.
#[must_use = "dropping the guard releases the lock at once"]
pub struct OwnedRwLockMappedWriteGuard<T: ?Sized, U: ?Sized = T> {
    lock: Arc<RwLock<T>>,
    data: *mut U,
    _p: PhantomData<T>,
}

// SAFETY: as for `OwnedRwLockWriteGuard`.
unsafe impl<T: ?Sized + Send + Sync, U: ?Sized + Send + Sync> Send
    for OwnedRwLockMappedWriteGuard<T, U>
{
}
// SAFETY: as for `OwnedRwLockWriteGuard`.
unsafe impl<T: ?Sized + Send + Sync, U: ?Sized + Send + Sync> Sync
    for OwnedRwLockMappedWriteGuard<T, U>
{
}

impl<T: ?Sized, U: ?Sized> OwnedRwLockMappedWriteGuard<T, U> {
    /// The `Arc`, once the guard is spent: releasing is left to whoever
    /// takes it over.
    fn into_lock(self) -> Arc<RwLock<T>> {
        let spent = ManuallyDrop::new(self);
        // SAFETY: `spent` is never dropped, so this is the only read of the
        // `Arc` and it is moved out exactly once.
        unsafe { ptr::read(&spent.lock) }
    }

    /// A guard for a component of the component. It cannot fail: this guard
    /// already holds the write.
    ///
    /// An associated function, `OwnedRwLockMappedWriteGuard::map(guard, f)`:
    /// a method would shadow methods of the locked data.
    #[inline]
    pub fn map<V, F>(mut this: Self, f: F) -> OwnedRwLockMappedWriteGuard<T, V>
    where
        V: ?Sized,
        F: FnOnce(&mut U) -> &mut V,
    {
        let data = ptr::from_mut(f(&mut *this));
        OwnedRwLockMappedWriteGuard {
            lock: this.into_lock(),
            data,
            _p: PhantomData,
        }
    }

    /// [`map`](Self::map) that gives the guard back when `f` finds no
    /// component, with the write still held.
    #[inline]
    pub fn try_map<V, F>(mut this: Self, f: F) -> Result<OwnedRwLockMappedWriteGuard<T, V>, Self>
    where
        V: ?Sized,
        F: FnOnce(&mut U) -> Option<&mut V>,
    {
        let data = match f(&mut *this) {
            Some(data) => ptr::from_mut(data),
            None => return Err(this),
        };
        Ok(OwnedRwLockMappedWriteGuard {
            lock: this.into_lock(),
            data,
            _p: PhantomData,
        })
    }

    /// The `Arc` of the lock this guard holds the write of.
    #[inline]
    pub fn rwlock(this: &Self) -> &Arc<RwLock<T>> {
        &this.lock
    }
}

impl<T: ?Sized, U: ?Sized> Deref for OwnedRwLockMappedWriteGuard<T, U> {
    type Target = U;

    fn deref(&self) -> &U {
        // SAFETY: the write excludes every other guard, and `data` points
        // into the value of the lock it holds.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized, U: ?Sized> DerefMut for OwnedRwLockMappedWriteGuard<T, U> {
    fn deref_mut(&mut self) -> &mut U {
        // SAFETY: as in `deref`, and `&mut self` makes this borrow unique.
        unsafe { &mut *self.data }
    }
}

impl<T: ?Sized, U: ?Sized> Drop for OwnedRwLockMappedWriteGuard<T, U> {
    fn drop(&mut self) {
        self.lock.raw.write_unlock();
    }
}

impl<T: ?Sized, U: ?Sized + fmt::Debug> fmt::Debug for OwnedRwLockMappedWriteGuard<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized, U: ?Sized + fmt::Display> fmt::Display for OwnedRwLockMappedWriteGuard<T, U> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}
