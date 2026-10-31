//! A first-come, first-served mutex for one shard: a one-permit
//! [`Semaphore`](super::Semaphore) around a value.
//!
//! An unlock with waiters hands the lock to the front waiter at once, so the
//! lock is never free while someone waits and nobody barges. It does not
//! poison: a panic in a holder unlocks it.

#![allow(unsafe_code)]

use core::cell::UnsafeCell;
use core::fmt;
use core::future::Future;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::task::{Context, Poll};

use super::semaphore::{Acquire, Semaphore};
use crate::sync::TryLockError;

/// A mutual-exclusion lock whose wait is a future, for the tasks of one shard.
///
/// Locking in arrival order: a [`lock`](Self::lock) that has to wait is
/// granted the lock by the unlock that frees it, and [`try_lock`](Self::try_lock)
/// fails while anyone waits. It is `!Send` and `!Sync`: share it between the
/// tasks of a shard with an `Rc`.
///
/// ```
/// use nuclei::sync::local::Mutex;
///
/// let counter = Mutex::new(0u32);
/// *counter.try_lock().expect("uncontended") += 1;
/// assert_eq!(counter.into_inner(), 1);
/// ```
pub struct Mutex<T: ?Sized> {
    sem: Semaphore,
    value: UnsafeCell<T>,
}

impl<T> Mutex<T> {
    /// An unlocked mutex holding `value`.
    pub const fn new(value: T) -> Self {
        Self {
            sem: Semaphore::new(1),
            value: UnsafeCell::new(value),
        }
    }

    /// Consumes the mutex and returns its value.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Waits for the lock, and answers a guard that unlocks when dropped.
    pub fn lock(&self) -> Lock<'_, T> {
        Lock {
            mutex: self,
            acquire: self.sem.acquire(),
        }
    }

    /// Locks the mutex if it is free and nobody waits.
    ///
    /// # Errors
    ///
    /// [`TryLockError`] when the mutex is locked or owed to a waiter.
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError> {
        match self.sem.try_acquire() {
            Ok(permit) => {
                permit.forget();
                Ok(MutexGuard { mutex: self })
            }
            Err(_) => Err(TryLockError(())),
        }
    }

    /// The value, through a unique borrow: no lock is needed.
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

/// The wait of [`Mutex::lock`].
///
/// Cancel safe: dropping it gives up its place in line; if the lock was
/// already handed to it, the lock passes to the next waiter.
#[must_use = "futures do nothing unless polled"]
pub struct Lock<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
    acquire: Acquire<'a>,
}

impl<'a, T: ?Sized> Future for Lock<'a, T> {
    type Output = MutexGuard<'a, T>;

    #[expect(
        clippy::unreachable,
        reason = "the mutex's semaphore is never closed, so its acquire never fails"
    )]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.acquire).poll(cx) {
            Poll::Ready(Ok(permit)) => {
                permit.forget();
                Poll::Ready(MutexGuard { mutex: this.mutex })
            }
            Poll::Ready(Err(_)) => unreachable!("the mutex's semaphore is never closed"),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: ?Sized> fmt::Debug for Lock<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lock").finish_non_exhaustive()
    }
}

/// Holds a [`Mutex`] locked; unlocks it when dropped.
#[must_use = "dropping the guard unlocks the mutex at once"]
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
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
    fn drop(&mut self) {
        self.mutex.sem.release(1);
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for MutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}
