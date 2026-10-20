//! A cell written once, with synchronous, racing and asynchronous ways to
//! fill it.
//!
//! The value lives behind one atomic pointer, so publishing it is a single
//! compare-and-swap and reading it is a single load: nobody ever sees a
//! half-written value, and nobody waits for a writer to finish. The
//! asynchronous initializers take turns through a one-permit semaphore, so
//! exactly one runs at a time, and a dropped or failed one gives the turn
//! up, which wakes the next waiter. Whoever publishes (an initializer,
//! `set` or a racing builder) opens the semaphore wide, which wakes every
//! task queued for a turn at once to find the value.

#![allow(unsafe_code)]

use core::convert::Infallible;
use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::ptr;

use super::internal::{AtomicPtr, Ordering};
use super::raw_semaphore::{MAX, RawSemaphore};

/// Permits the publisher adds: more than the tasks that can ever be
/// queued, and far enough from the limit that each one taking and
/// returning a permit cannot overflow it.
const OPEN: usize = MAX / 2;

/// A cell that is written at most once.
///
/// - [`set`](Self::set) publishes a value with one compare-and-swap and
///   never waits.
/// - [`get_or_init_racy`](Self::get_or_init_racy) lets every caller that
///   finds the cell empty build a value; one compare-and-swap publishes
///   one of them and the others are dropped.
/// - [`get_or_init`](Self::get_or_init) and
///   [`get_or_try_init`](Self::get_or_try_init) run one initializer at a
///   time; the others wait for it as futures. An initializer that fails
///   or is dropped passes the turn to the next waiter.
///
/// ```
/// use nuclei::sync::OnceCell;
///
/// static NAME: OnceCell<String> = OnceCell::new();
/// assert!(NAME.set("first".to_owned()).is_ok());
/// assert_eq!(NAME.set("second".to_owned()), Err("second".to_owned()));
/// assert_eq!(NAME.get_or_init_racy(|| "unused".to_owned()), "first");
/// ```
pub struct OnceCell<T> {
    value: AtomicPtr<T>,
    init: RawSemaphore,
    // Owns a `T`; `Sync` is granted below only when `T: Send + Sync`.
    _value: PhantomData<core::cell::UnsafeCell<T>>,
}

// SAFETY: readers on any thread share `&T`, and the thread that drops the
// cell drops a `T` that another thread may have built.
unsafe impl<T: Send + Sync> Sync for OnceCell<T> {}

impl<T> OnceCell<T> {
    loom_const_fn! {
        /// An empty cell.
        pub fn new() -> Self {
            Self {
                value: AtomicPtr::new(ptr::null_mut()),
                // The turn is held across the caller's initializer, which may
                // await anything: a waiter queues at once.
                init: RawSemaphore::without_spin(1),
                _value: PhantomData,
            }
        }
    }

    /// The value, once published.
    pub fn get(&self) -> Option<&T> {
        // SAFETY: a published value is never moved or freed while `&self`
        // lives.
        unsafe { self.value.load(Ordering::Acquire).as_ref() }
    }

    /// The value, borrowed mutably.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        // SAFETY: `&mut self` makes this the only access.
        unsafe { self.value.load(Ordering::Acquire).as_mut() }
    }

    /// Consumes the cell and returns its value.
    pub fn into_inner(self) -> Option<T> {
        let value = self.value.swap(ptr::null_mut(), Ordering::Acquire);
        // SAFETY: a published value is a leaked `Box` only the cell owns.
        (!value.is_null()).then(|| *unsafe { Box::from_raw(value) })
    }

    /// Publishes `value` unless the cell already holds one, in which case
    /// `value` comes back. One compare-and-swap publishes; never waits. A
    /// successful publish also releases any task waiting in
    /// [`get_or_init`](Self::get_or_init).
    pub fn set(&self, value: T) -> Result<(), T> {
        if self.get().is_some() {
            return Err(value);
        }
        self.try_publish(value)
            .map(drop)
            .map_err(|(value, _)| value)
    }

    /// The value, building one with `init` if the cell is empty. Callers
    /// that race all build; one value is published and the rest are
    /// dropped. Never waits.
    pub fn get_or_init_racy(&self, init: impl FnOnce() -> T) -> &T {
        match self.get() {
            Some(value) => value,
            None => self.publish(init()),
        }
    }

    /// The value, running `init` to build it if the cell is empty. One
    /// initializer runs at a time; the rest wait.
    pub async fn get_or_init<F>(&self, init: F) -> &T
    where
        F: Future<Output = T>,
    {
        match self
            .get_or_try_init(async { Ok::<T, Infallible>(init.await) })
            .await
        {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }

    /// The value, running `init` to build it if the cell is empty. One
    /// initializer runs at a time; when it fails, its error is returned
    /// and the next waiter tries its own.
    pub async fn get_or_try_init<E, F>(&self, init: F) -> Result<&T, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        if let Some(value) = self.get() {
            return Ok(value);
        }
        // Dropped, failed or done, the turn returns its permit, so the next
        // waiter takes over or finds the value.
        let _turn = self.init.acquire(1).await;
        if let Some(value) = self.get() {
            return Ok(value);
        }
        Ok(self.publish(init.await?))
    }

    /// Publishes `value`, or drops it and returns the value that won.
    fn publish(&self, value: T) -> &T {
        match self.try_publish(value) {
            Ok(value) => value,
            Err((_, winner)) => winner,
        }
    }

    /// Publishes `value` with one compare-and-swap. Whoever publishes,
    /// an initializer, `set` or a racing builder, then opens the turn
    /// queue, so a task waiting behind an initializer that is still
    /// running sees the value now rather than when that initializer ends.
    /// On failure `value` comes back with the value that won.
    fn try_publish(&self, value: T) -> Result<&T, (T, &T)> {
        let fresh = Box::into_raw(Box::new(value));
        match self.value.compare_exchange(
            ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // One publish per cell, onto at most the one permit the
                // cell started with, so this cannot overflow.
                self.init.release(OPEN);
                // SAFETY: published; it lives as long as the cell.
                Ok(unsafe { &*fresh })
            }
            // SAFETY: `fresh` lost the race and was never shared; the
            // winner is published and lives as long as the cell.
            Err(winner) => Err((*unsafe { Box::from_raw(fresh) }, unsafe { &*winner })),
        }
    }
}

impl<T> Drop for OnceCell<T> {
    fn drop(&mut self) {
        let value = self.value.load(Ordering::Acquire);
        if !value.is_null() {
            // SAFETY: a published value is a leaked `Box` only the cell owns.
            drop(unsafe { Box::from_raw(value) });
        }
    }
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for OnceCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OnceCell").field(&self.get()).finish()
    }
}

impl<T> From<T> for OnceCell<T> {
    fn from(value: T) -> Self {
        let cell = Self::new();
        let _ = cell.set(value);
        cell
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::super::queue::Policy;
    use super::*;
    use crate::sync::Event;
    use crate::sync::test_support::Polled;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};

    /// A caller waiting for the initializer turn queues at once and never
    /// spins: the turn is held across the caller's initializer, which may
    /// await anything. The turn's spin budget stays at zero through a
    /// contended wait, its wake and its retry.
    #[test]
    fn a_waiter_for_the_initializer_turn_never_spins() {
        let cell = OnceCell::<u8>::new();
        assert_eq!(cell.init.queue().spin.rounds(), 0);
        let opened = Event::new();
        let mut first = Polled::new(cell.get_or_init(async {
            opened.wait().await;
            1
        }));
        first.pending();
        let mut second = Polled::new(cell.get_or_init(async { 2 }));
        second.pending();
        opened.set();
        assert_eq!(*first.ready(), 1);
        assert_eq!(*second.ready(), 1);
        assert_eq!(cell.init.queue().spin.rounds(), 0, "the turn never spins");
    }

    #[test]
    fn set_publishes_once() {
        let cell = OnceCell::new();
        assert_eq!(cell.get(), None);
        assert_eq!(cell.set(1), Ok(()));
        assert_eq!(cell.set(2), Err(2));
        assert_eq!(cell.get(), Some(&1));
        assert_eq!(cell.into_inner(), Some(1));
    }

    #[test]
    fn racing_builders_publish_exactly_one_value() {
        let cell = Arc::new(OnceCell::new());
        let built = Arc::new(AtomicUsize::new(0));
        let seen: Vec<usize> = (0..4)
            .map(|i| {
                let (cell, built) = (Arc::clone(&cell), Arc::clone(&built));
                std::thread::spawn(move || {
                    *cell.get_or_init_racy(|| {
                        built.fetch_add(1, StdOrdering::SeqCst);
                        i
                    })
                })
            })
            .map(|thread| thread.join().expect("racer"))
            .collect();
        assert!(seen.iter().all(|v| *v == seen[0]), "{seen:?}");
        assert!(built.load(StdOrdering::SeqCst) >= 1);
    }

    #[test]
    fn one_initializer_runs_while_the_others_wait() {
        let cell = OnceCell::new();
        let opened = Event::new();
        let mut first = Polled::new(cell.get_or_init(async {
            opened.wait().await;
            1
        }));
        first.pending();
        let mut second = Polled::new(cell.get_or_init(async { 2 }));
        second.pending();
        opened.set();
        assert_eq!(*first.ready(), 1);
        assert_eq!(second.wakes(), 1);
        assert_eq!(*second.ready(), 1);
    }

    #[test]
    fn a_set_releases_waiters_queued_behind_a_running_initializer() {
        let cell = OnceCell::new();
        let mut stuck = Polled::new(cell.get_or_init(core::future::pending()));
        stuck.pending();
        let mut waiting = Polled::new(cell.get_or_init(async { 2 }));
        waiting.pending();
        assert_eq!(cell.set(1), Ok(()));
        assert_eq!(waiting.wakes(), 1, "the publisher wakes the queue");
        assert_eq!(*waiting.ready(), 1);
        let mut racy_waiter = Polled::new(cell.get_or_init(async { 3 }));
        assert_eq!(*racy_waiter.ready(), 1);
        drop(stuck);
    }

    #[test]
    fn a_dropped_initializer_passes_the_turn_on() {
        let cell = OnceCell::new();
        let mut first = Polled::new(cell.get_or_init(core::future::pending()));
        first.pending();
        let mut second = Polled::new(cell.get_or_init(async { 2 }));
        second.pending();
        drop(first);
        assert_eq!(second.wakes(), 1);
        assert_eq!(*second.ready(), 2);
    }

    #[test]
    fn a_failed_initializer_lets_the_next_one_try() {
        let cell = OnceCell::new();
        let opened = Event::new();
        let mut failing = Polled::new(cell.get_or_try_init(async {
            opened.wait().await;
            Err::<u32, &str>("no")
        }));
        failing.pending();
        let mut next = Polled::new(cell.get_or_try_init(async { Ok::<u32, &str>(3) }));
        next.pending();
        opened.set();
        assert_eq!(failing.ready(), Err("no"));
        assert_eq!(next.ready(), Ok(&3));
    }
}
