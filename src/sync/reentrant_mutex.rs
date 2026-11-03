//! A fair asynchronous mutex its holder may lock again, keyed by
//! [`Owner`].

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::ops::Deref;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::internal::{AtomicU64, Ordering, UnsafeCell};
use super::owner::Owner;
use super::raw_semaphore::{PermitWait, RawSemaphore};
use super::siblings::{Ticket, wake_siblings};

/// A mutex the owner holding it can lock again without waiting.
///
/// Each lock by the holding owner deepens a count; the mutex unlocks when
/// the last of its guards drops, and then wakes the oldest waiting
/// owner, with the bounded bypass of [`Mutex`](super::Mutex).
///
/// A guard gives `&T`, since two guards of one owner are alive at once;
/// mutate through a `Cell` or `RefCell` inside. Guards and lock futures
/// borrow their [`Owner`], so they stay on its thread (see the `Owner`
/// docs): a task that holds or awaits one is not `Send` meanwhile.
///
/// ```
/// use nuclei::sync::{Owner, ReentrantMutex};
/// use std::cell::Cell;
///
/// let lock = ReentrantMutex::new(Cell::new(0));
/// let me = Owner::new();
/// let a = lock.try_lock(&me).expect("free");
/// let b = lock.try_lock(&me).expect("re-entered");
/// b.set(a.get() + 1);
/// drop((a, b));
/// assert_eq!(lock.into_inner().get(), 1);
/// ```
pub struct ReentrantMutex<T: ?Sized> {
    sem: RawSemaphore,
    /// The holding owner's id, or zero. Only the holder stores its own id,
    /// so a holder reading its own id back is the only way to match.
    holder: AtomicU64,
    /// How many guards the holder has; touched only by the holder.
    depth: UnsafeCell<usize>,
    value: core::cell::UnsafeCell<T>,
}

// SAFETY: the value moves with the lock.
unsafe impl<T: ?Sized + Send> Send for ReentrantMutex<T> {}
// SAFETY: the semaphore admits one owner at a time and every guard of that
// owner is on its thread (see `Owner`), so `&T` never reaches two threads
// at once and `T: Sync` is not needed.
unsafe impl<T: ?Sized + Send> Sync for ReentrantMutex<T> {}

impl<T> ReentrantMutex<T> {
    loom_const_fn! {
        /// An unlocked mutex holding `value`.
        pub fn new(value: T) -> Self {
            Self {
                sem: RawSemaphore::new(1),
                holder: AtomicU64::new(0),
                depth: UnsafeCell::new(0),
                value: core::cell::UnsafeCell::new(value),
            }
        }
    }

    /// Consumes the mutex and returns its value.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> ReentrantMutex<T> {
    /// Locks the mutex if `owner` holds it, or if it is free and no
    /// handoff is owed.
    pub fn try_lock<'a>(&'a self, owner: &'a Owner) -> Option<ReentrantMutexGuard<'a, T>> {
        if self.reenter(owner) || self.sem.try_acquire(1) && self.enter(owner, 0) {
            return Some(ReentrantMutexGuard::new(self));
        }
        None
    }

    /// Waits for the lock, at once if `owner` already holds it. A
    /// contended wait is lock-free, not wait-free: see
    /// [Progress](super#progress).
    ///
    /// Every poll looks for `owner` first, and a wait that queued before
    /// `owner` took the lock through another of its requests (two locks in
    /// one `join!`, or in two tasks of one owner) is woken by that entry, so
    /// it leaves the queue and enters at once.
    pub fn lock<'a>(&'a self, owner: &'a Owner) -> ReentrantMutexLock<'a, T> {
        ReentrantMutexLock {
            mutex: self,
            owner,
            wait: PermitWait::new(),
            ticket: Ticket::new(),
        }
    }

    /// The value, borrowed mutably; `&mut self` proves no guard exists.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Deepens the hold if `owner` has it.
    #[expect(
        clippy::panic,
        reason = "a count that cannot overflow in practice, checked rather than wrapped"
    )]
    fn reenter(&self, owner: &Owner) -> bool {
        if self.holder.load(Ordering::Relaxed) != owner.id() {
            return false;
        }
        // SAFETY: only the holder touches the depth, and this is it.
        self.depth.with_mut(|depth| unsafe {
            *depth = (*depth)
                .checked_add(1)
                .unwrap_or_else(|| panic!("a ReentrantMutex was locked usize::MAX times"));
        });
        true
    }

    /// Records `owner` as the holder after the permit was taken, and wakes
    /// its other queued requests on this lock (all but ticket `except`).
    fn enter(&self, owner: &Owner, except: u64) -> bool {
        // SAFETY: the permit makes this caller the only holder.
        self.depth.with_mut(|depth| unsafe { *depth = 1 });
        self.holder.store(owner.id(), Ordering::Relaxed);
        wake_siblings(self.address(), owner.id(), except);
        true
    }

    /// The lock's identity in the registry of queued requests.
    fn address(&self) -> usize {
        core::ptr::from_ref(self).cast::<()>() as usize
    }

    fn unlock(&self) {
        // SAFETY: only the holder drops a guard, and only it touches the
        // depth.
        let left = self.depth.with_mut(|depth| unsafe {
            *depth -= 1;
            *depth
        });
        if left == 0 {
            self.holder.store(0, Ordering::Relaxed);
            self.sem.release(1);
        }
    }
}

impl<T: Default> Default for ReentrantMutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: ?Sized> fmt::Debug for ReentrantMutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReentrantMutex").finish_non_exhaustive()
    }
}

/// Shared access to a [`ReentrantMutex`]'s value; drops one level of the
/// hold. The drop of the last level releases the lock, which is lock-free,
/// not wait-free: with waiters it returns after one pass over the queue,
/// plus at most one for each operation of another thread that lands on the
/// queue meanwhile (see [Progress](super#progress)).
#[must_use = "dropping the guard releases one level of the lock at once"]
pub struct ReentrantMutexGuard<'a, T: ?Sized> {
    mutex: &'a ReentrantMutex<T>,
    // Borrowing an `Owner`, which is not `Sync`, keeps the guard on the
    // owner's thread.
    _owner: PhantomData<&'a Owner>,
}

impl<'a, T: ?Sized> ReentrantMutexGuard<'a, T> {
    fn new(mutex: &'a ReentrantMutex<T>) -> Self {
        Self {
            mutex,
            _owner: PhantomData,
        }
    }
}

impl<T: ?Sized> Deref for ReentrantMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the holder's guards are the only ones and they are all
        // on its thread, so shared access cannot race.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> Drop for ReentrantMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for ReentrantMutexGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

/// The future [`ReentrantMutex::lock`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct ReentrantMutexLock<'a, T: ?Sized> {
    mutex: &'a ReentrantMutex<T>,
    owner: &'a Owner,
    wait: PermitWait,
    /// Its place among the owner's queued requests.
    ticket: Ticket,
}

impl<'a, T: ?Sized> Future for ReentrantMutexLock<'a, T> {
    type Output = ReentrantMutexGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<ReentrantMutexGuard<'a, T>> {
        let this = self.get_mut();
        if this.mutex.reenter(this.owner) {
            // The owner took the lock through another request since this one
            // queued: this one leaves the queue, which cannot have granted it
            // the permit the owner holds.
            // The guard first: the cancel and the budget's refusal run user
            // code, and a panic there gives the level back.
            let guard = ReentrantMutexGuard::new(this.mutex);
            this.wait.cancel(&this.mutex.sem, 1);
            this.ticket.unregister();
            // Completed at once: it spends one operation of the poll's
            // budget, or, the budget spent, the level goes and it yields.
            if crate::coop::spend(cx.waker()) {
                return Poll::Ready(guard);
            }
            drop(guard);
            return Poll::Pending;
        }
        match this.wait.poll(&this.mutex.sem, 1, cx.waker()) {
            Poll::Ready(()) => {
                this.mutex.enter(this.owner, this.ticket.number());
                this.ticket.unregister();
                Poll::Ready(ReentrantMutexGuard::new(this.mutex))
            }
            Poll::Pending => {
                if this.wait.is_queued() {
                    this.ticket
                        .register(this.mutex.address(), this.owner.id(), cx.waker());
                }
                Poll::Pending
            }
        }
    }
}

impl<T: ?Sized> Drop for ReentrantMutexLock<'_, T> {
    fn drop(&mut self) {
        self.wait.cancel(&self.mutex.sem, 1);
    }
}

impl<T: ?Sized> fmt::Debug for ReentrantMutexLock<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReentrantMutexLock")
            .field("owner", self.owner)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::Polled;
    use core::cell::RefCell;

    #[test]
    fn the_holder_reenters_and_the_lock_opens_after_its_last_guard() {
        let lock = ReentrantMutex::new(RefCell::new(Vec::new()));
        let (me, other) = (Owner::new(), Owner::new());
        let outer = lock.try_lock(&me).expect("free");
        let inner = Polled::new(lock.lock(&me)).ready();
        inner.borrow_mut().push(1);
        let mut waiting = Polled::new(lock.lock(&other));
        waiting.pending();
        assert!(lock.try_lock(&other).is_none());
        drop(inner);
        assert_eq!(waiting.wakes(), 0, "one level is still held");
        let again = lock.try_lock(&me).expect("still the holder");
        drop((outer, again));
        assert_eq!(waiting.wakes(), 1);
        let theirs = waiting.ready();
        theirs.borrow_mut().push(2);
        assert!(lock.try_lock(&me).is_none(), "the lock changed hands");
        drop(theirs);
        drop(waiting);
        assert_eq!(lock.into_inner().into_inner(), vec![1, 2]);
    }

    #[test]
    fn a_thread_owner_is_the_same_on_one_thread_and_different_on_another() {
        let lock = ReentrantMutex::new(());
        let here = Owner::thread();
        let guard = lock.try_lock(&here).expect("free");
        let again = Owner::thread();
        assert!(lock.try_lock(&again).is_some(), "the same thread re-enters");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let there = Owner::thread();
                assert!(
                    lock.try_lock(&there).is_none(),
                    "another thread is another owner"
                );
            });
        });
        drop(guard);
    }

    #[test]
    fn a_queued_lock_of_the_owner_that_took_the_lock_meanwhile_enters_at_once() {
        let lock = ReentrantMutex::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let theirs = lock.try_lock(&other).expect("free");
        let mut first = Polled::new(lock.lock(&me));
        let mut second = Polled::new(lock.lock(&me));
        first.pending();
        second.pending();
        drop(theirs);
        let a = first.ready();
        let b = second.ready();
        drop((a, b));
        assert_eq!(lock.sem.linked(), 0, "the second lock left the queue");
        assert!(lock.try_lock(&other).is_some(), "both levels went");
    }

    #[test]
    fn a_withdrawn_lock_future_leaves_no_trace() {
        let lock = ReentrantMutex::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let held = lock.try_lock(&me).expect("free");
        Polled::new(lock.lock(&other)).pending();
        drop(held);
        assert!(lock.try_lock(&other).is_some());
    }

    #[test]
    fn a_reenter_whose_cancel_panics_keeps_no_level() {
        let lock = ReentrantMutex::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let theirs = lock.try_lock(&other).expect("free");
        let mut first = Polled::new(lock.lock(&me));
        let waker = crate::sync::test_support::drop_panics();
        let mut second = Box::pin(lock.lock(&me));
        let mut cx = Context::from_waker(&waker);
        first.pending();
        assert!(second.as_mut().poll(&mut cx).is_pending());
        drop(theirs);
        let first = first.ready();
        // The owner holds the lock through `first`, so the second request
        // re-enters; leaving the queue drops its waker, which panics.
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            second.as_mut().poll(&mut cx)
        }));
        assert!(caught.is_err(), "the panic reaches the caller");
        drop(first);
        drop(second);
        assert!(lock.try_lock(&other).is_some(), "a level was kept");
    }

    #[test]
    fn an_entry_wakes_the_younger_request_of_its_owner_that_was_polled_first() {
        let lock = ReentrantMutex::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let theirs = lock.try_lock(&other).expect("free");
        let mut elder = Polled::new(lock.lock(&me));
        let mut younger = Polled::new(lock.lock(&me));
        elder.pending();
        younger.pending();
        drop(theirs);
        // A `join!` polls the younger first: the elder holds the grant.
        younger.pending();
        assert_eq!(younger.wakes(), 0);
        let held = elder.ready();
        assert_eq!(
            younger.wakes(),
            1,
            "the owner's entry woke its other request"
        );
        let again = younger.ready();
        assert!(lock.try_lock(&other).is_none());
        drop((held, again));
        assert!(lock.try_lock(&other).is_some(), "both levels went");
    }

    #[test]
    fn two_requests_of_a_thread_owner_do_not_wait_on_each_other() {
        let lock = ReentrantMutex::new(());
        let (me, same, other) = (Owner::thread(), Owner::thread(), Owner::new());
        let theirs = lock.try_lock(&other).expect("free");
        let mut first = Polled::new(lock.lock(&me));
        let mut second = Polled::new(lock.lock(&same));
        first.pending();
        second.pending();
        drop(theirs);
        second.pending();
        let held = first.ready();
        assert_eq!(second.wakes(), 1);
        let again = second.ready();
        drop((held, again));
        assert!(lock.try_lock(&other).is_some());
    }

    #[test]
    fn a_dropped_request_leaves_the_registry() {
        let lock = ReentrantMutex::new(());
        let (me, other) = (Owner::new(), Owner::new());
        let theirs = lock.try_lock(&other).expect("free");
        let mut waiting = Polled::new(lock.lock(&me));
        waiting.pending();
        let mut sibling = Polled::new(lock.lock(&me));
        sibling.pending();
        drop(waiting);
        drop(theirs);
        let held = sibling.ready();
        assert_eq!(crate::sync::siblings::queued_here(), 0);
        drop(held);
    }
}
