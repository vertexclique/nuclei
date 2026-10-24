//! A cell written once, with synchronous, racing and asynchronous ways to
//! fill it.
//!
//! The value lives in one of two places, and one atomic pointer says which:
//! null while the cell is empty, a marker address once the value sits in the
//! cell's own slot, or a leaked box once a racing builder published one. So
//! publishing is a single compare-and-swap, reading is a single load, and
//! nobody ever sees a half-written value or waits for a writer to finish.
//!
//! The turn to write the slot goes through a one-permit semaphore: `set`
//! and the asynchronous initializers take it, so exactly one writes at a
//! time, and a dropped or failed initializer gives the turn up, which wakes
//! the next waiter. Whoever publishes (an initializer, `set` or a racing
//! builder) then closes the semaphore, which wakes every task queued for a
//! turn at once to find the value. A racing builder does not take the turn:
//! it builds a box off to the side and lets one compare-and-swap decide.

#![allow(unsafe_code)]

use core::convert::Infallible;
use core::error::Error;
use core::fmt;
use core::future::Future;
use core::mem::{self, MaybeUninit};
use core::ptr;

use super::contend::Closed;
use super::internal::{AtomicPtr, Ordering, UnsafeCell, take_ptr};
use super::raw_semaphore::{RawPermit, RawSemaphore, observed_closed};

/// The stored pointer that says the value sits in the cell's slot. No
/// allocation, and no zero-sized value's dangling box, can have this
/// address, so it never stands for a box.
const fn inline_marker<T>() -> *mut T {
    ptr::without_provenance_mut(usize::MAX)
}

/// Errors that can be returned from [`OnceCell::set`].
#[derive(Debug, PartialEq, Eq)]
pub enum SetError<T> {
    /// The cell was already initialized when [`OnceCell::set`] was called.
    AlreadyInitializedError(T),

    /// The cell is currently being initialized.
    InitializingError(T),
}

impl<T> fmt::Display for SetError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitializedError(_) => f.write_str("AlreadyInitializedError"),
            Self::InitializingError(_) => f.write_str("InitializingError"),
        }
    }
}

impl<T: fmt::Debug> Error for SetError<T> {}

impl<T> SetError<T> {
    /// Whether this is [`SetError::AlreadyInitializedError`].
    pub fn is_already_init_err(&self) -> bool {
        matches!(self, Self::AlreadyInitializedError(_))
    }

    /// Whether this is [`SetError::InitializingError`].
    pub fn is_initializing_err(&self) -> bool {
        matches!(self, Self::InitializingError(_))
    }
}

/// A cell that is written at most once.
///
/// - [`set`](Self::set) publishes a value with one compare-and-swap and
///   never waits. It fails with [`SetError::InitializingError`] while an
///   asynchronous initializer holds the turn.
/// - [`get_or_init_racy`](Self::get_or_init_racy) lets every caller that
///   finds the cell empty build a value; one compare-and-swap publishes
///   one of them and the others are dropped.
/// - [`get_or_init`](Self::get_or_init) and
///   [`get_or_try_init`](Self::get_or_try_init) run one initializer at a
///   time; the others wait for it as futures. An initializer that fails
///   or is dropped passes the turn to the next waiter.
///
/// ```
/// use nuclei::sync::{OnceCell, SetError};
///
/// static NAME: OnceCell<String> = OnceCell::const_new();
/// assert!(NAME.set("first".to_owned()).is_ok());
/// assert_eq!(
///     NAME.set("second".to_owned()),
///     Err(SetError::AlreadyInitializedError("second".to_owned()))
/// );
/// assert_eq!(NAME.get_or_init_racy(|| "unused".to_owned()), "first");
/// ```
pub struct OnceCell<T> {
    /// Null while empty; [`inline_marker`] once the value sits in `slot`;
    /// otherwise a leaked `Box<T>` a racing builder published.
    value: AtomicPtr<T>,
    /// The value, written while its writer holds the turn and published by
    /// the marker.
    slot: UnsafeCell<MaybeUninit<T>>,
    /// The turn to write `slot`: one permit until the cell holds a value,
    /// then closed.
    init: RawSemaphore,
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
                slot: UnsafeCell::new(MaybeUninit::uninit()),
                // The turn is held across the caller's initializer, which may
                // await anything: a waiter queues at once.
                init: RawSemaphore::without_spin(1),
            }
        }
    }

    loom_const_fn! {
        /// An empty cell, usable in a `static`.
        pub fn const_new() -> Self {
            Self::new()
        }
    }

    /// A cell that holds `value` if there is one, and is empty otherwise.
    pub fn new_with(value: Option<T>) -> Self {
        match value {
            Some(value) => Self::from(value),
            None => Self::new(),
        }
    }

    loom_const_fn! {
        /// A cell that already holds `value`, usable in a `static`.
        pub fn const_new_with(value: T) -> Self {
            Self {
                value: AtomicPtr::new(inline_marker()),
                slot: UnsafeCell::new(MaybeUninit::new(value)),
                init: RawSemaphore::new_closed(),
            }
        }
    }

    /// Whether the cell holds a value.
    pub fn initialized(&self) -> bool {
        // Acquire, so a thread that sees the value published can read it.
        !self.value.load(Ordering::Acquire).is_null()
    }

    /// The value `stored` stands for, which is not null.
    fn resolve(&self, stored: *mut T) -> &T {
        if stored == inline_marker() {
            // SAFETY: the marker is published after the slot was written and
            // the slot is never written again.
            unsafe { &*self.slot.with(|slot| slot.cast::<T>()) }
        } else {
            // SAFETY: a published box is never moved or freed while `&self`
            // lives.
            unsafe { &*stored }
        }
    }

    /// The value, once published.
    pub fn get(&self) -> Option<&T> {
        let stored = self.value.load(Ordering::Acquire);
        (!stored.is_null()).then(|| self.resolve(stored))
    }

    /// The value, borrowed mutably.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        let stored = self.value.load(Ordering::Acquire);
        if stored.is_null() {
            None
        } else if stored == inline_marker() {
            // SAFETY: `&mut self` makes this the only access, and the marker
            // says the slot holds a value.
            Some(unsafe { &mut *self.slot.with_mut(|slot| slot.cast::<T>()) })
        } else {
            // SAFETY: `&mut self` makes this the only access to the box.
            Some(unsafe { &mut *stored })
        }
    }

    /// Moves the value out and leaves the cell empty of it. Whoever calls
    /// it has the cell to itself.
    fn take_value(&mut self) -> Option<T> {
        let stored = take_ptr(&mut self.value);
        if stored.is_null() {
            None
        } else if stored == inline_marker() {
            // SAFETY: the marker said the slot holds a value, and the
            // pointer is null now, so nothing reads or drops the slot again.
            Some(unsafe { self.slot.with_mut(|slot| (*slot).assume_init_read()) })
        } else {
            // SAFETY: a published box is a leaked `Box` only the cell owns,
            // and the pointer is null now.
            Some(*unsafe { Box::from_raw(stored) })
        }
    }

    /// Consumes the cell and returns its value, if it has one.
    pub fn into_inner(mut self) -> Option<T> {
        self.take_value()
    }

    /// Takes the value out of the cell, leaving it empty and ready to be set
    /// again. `None` if the cell is empty.
    pub fn take(&mut self) -> Option<T> {
        mem::take(self).into_inner()
    }

    /// Publishes `value` unless the cell already holds one or a task is
    /// initializing it, in which case `value` comes back in the error. One
    /// compare-and-swap publishes; never waits. A successful publish also
    /// releases any task waiting in [`get_or_init`](Self::get_or_init).
    ///
    /// # Errors
    ///
    /// [`SetError::AlreadyInitializedError`] when the cell holds a value,
    /// [`SetError::InitializingError`] when an initializer holds the turn.
    pub fn set(&self, value: T) -> Result<(), SetError<T>> {
        if self.initialized() {
            return Err(SetError::AlreadyInitializedError(value));
        }
        match self.init.try_acquire_permit(1) {
            Ok(turn) => self
                .publish_inline(value, turn)
                .map(drop)
                .map_err(|(value, _)| SetError::AlreadyInitializedError(value)),
            // The turn queue closes only once the cell holds a value.
            Err(seen) if observed_closed(seen) => Err(SetError::AlreadyInitializedError(value)),
            // Some task holds the turn, or is owed it: it is initializing.
            Err(_) => Err(SetError::InitializingError(value)),
        }
    }

    /// The value, building one with `init` if the cell is empty. Callers
    /// that race all build; one value is published and the rest are
    /// dropped. Never waits. A racing builder does not take the turn, so it
    /// also wins over an initializer that is running: that one's value is
    /// dropped when it finishes.
    pub fn get_or_init_racy(&self, init: impl FnOnce() -> T) -> &T {
        match self.get() {
            Some(value) => value,
            None => self.publish_boxed(init()),
        }
    }

    /// The value, running `f` to build it if the cell is empty. One
    /// initializer runs at a time; the rest wait. `f` is called only by the
    /// task that holds the turn, and not at all if the cell is full by then.
    ///
    /// If the future `f` returns is dropped or panics, the attempt is
    /// cancelled and the next waiter makes its own.
    ///
    /// This waits for ever if `f` tries to initialize the cell recursively.
    pub async fn get_or_init<F, Fut>(&self, f: F) -> &T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        match self
            .get_or_try_init(|| async move { Ok::<T, Infallible>(f().await) })
            .await
        {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }

    /// The value, running `f` to build it if the cell is empty. One
    /// initializer runs at a time; when it fails, its error is returned and
    /// the next waiter tries its own.
    ///
    /// If the future `f` returns is dropped or panics, the attempt is
    /// cancelled and the next waiter makes its own.
    ///
    /// This waits for ever if `f` tries to initialize the cell recursively.
    pub async fn get_or_try_init<E, F, Fut>(&self, f: F) -> Result<&T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        if let Some(value) = self.get() {
            return Ok(value);
        }
        // Dropped, failed or done, the turn returns its permit, so the next
        // waiter takes over or finds the value.
        match self.init.acquire(1).await {
            Ok(turn) => {
                // A racing builder may have published while this waited.
                if let Some(value) = self.get() {
                    return Ok(value);
                }
                let value = f().await?;
                // When a racing builder published first, this value is
                // dropped and that one's is answered.
                Ok(match self.publish_inline(value, turn) {
                    Ok(value) | Err((_, value)) => value,
                })
            }
            // The queue closes when a value is published, so there is one to
            // answer. A closed queue over an empty cell is a state nothing
            // here makes; if it is met all the same, this call builds the
            // value itself, as a racing builder does, and answers it.
            Err(Closed) => match self.get() {
                Some(value) => Ok(value),
                None => Ok(self.publish_boxed(f().await?)),
            },
        }
    }

    /// Writes `value` to the slot and publishes it with one compare-and-swap,
    /// then closes the turn queue, so a task waiting behind an initializer
    /// sees the value now rather than when that initializer ends. The caller
    /// holds the turn. When a racing builder published first, `value` comes
    /// back with the value that won.
    fn publish_inline(&self, value: T, turn: RawPermit<'_>) -> Result<&T, (T, &T)> {
        // SAFETY: the turn makes this caller the slot's only writer, and
        // nobody reads the slot before the marker below is published.
        self.slot.with_mut(|slot| unsafe {
            (*slot).write(value);
        });
        match self.value.compare_exchange(
            ptr::null_mut(),
            inline_marker(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                turn.forget();
                self.init.close();
                Ok(self.resolve(inline_marker()))
            }
            Err(winner) => Err((
                // SAFETY: the slot was never published, so this caller still
                // owns what it wrote, and moving it out leaves nothing to drop.
                unsafe { self.slot.with_mut(|slot| (*slot).assume_init_read()) },
                self.resolve(winner),
            )),
        }
    }

    /// Publishes `value` in a box with one compare-and-swap, or drops it and
    /// returns the value that won. A publish closes the turn queue.
    fn publish_boxed(&self, value: T) -> &T {
        let fresh = Box::into_raw(Box::new(value));
        match self.value.compare_exchange(
            ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.init.close();
                // SAFETY: published; it lives as long as the cell.
                unsafe { &*fresh }
            }
            Err(winner) => {
                // SAFETY: `fresh` lost the race and was never shared.
                drop(unsafe { Box::from_raw(fresh) });
                self.resolve(winner)
            }
        }
    }
}

impl<T> Drop for OnceCell<T> {
    fn drop(&mut self) {
        drop(self.take_value());
    }
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for OnceCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnceCell")
            .field("value", &self.get())
            .finish()
    }
}

impl<T: Clone> Clone for OnceCell<T> {
    fn clone(&self) -> Self {
        Self::new_with(self.get().cloned())
    }
}

impl<T: PartialEq> PartialEq for OnceCell<T> {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl<T: Eq> Eq for OnceCell<T> {}

impl<T> From<T> for OnceCell<T> {
    fn from(value: T) -> Self {
        Self::const_new_with(value)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::super::queue::Policy;
    use super::*;
    use crate::sync::Event;
    use crate::sync::test_support::{Polled, block_on};
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
        let mut first = Polled::new(cell.get_or_init(|| async {
            opened.wait().await;
            1
        }));
        first.pending();
        let mut second = Polled::new(cell.get_or_init(|| async { 2 }));
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
        assert!(!cell.initialized());
        assert_eq!(cell.set(1), Ok(()));
        assert!(cell.initialized());
        assert_eq!(cell.set(2), Err(SetError::AlreadyInitializedError(2)));
        assert_eq!(cell.get(), Some(&1));
        assert_eq!(cell.into_inner(), Some(1));
    }

    #[test]
    fn set_fails_while_an_initializer_holds_the_turn() {
        let cell = OnceCell::new();
        let mut running = Polled::new(cell.get_or_init(core::future::pending::<u8>));
        running.pending();
        let err = cell.set(3).unwrap_err();
        assert!(err.is_initializing_err() && !err.is_already_init_err());
        assert_eq!(err, SetError::InitializingError(3));
        assert_eq!(err.to_string(), "InitializingError");
        assert_eq!(cell.get(), None);
        drop(running);
        assert_eq!(cell.set(3), Ok(()), "the turn was given up");
        let err = cell.set(4).unwrap_err();
        assert!(err.is_already_init_err());
        assert_eq!(err.to_string(), "AlreadyInitializedError");
    }

    #[test]
    fn a_racing_builder_publishes_beside_a_running_initializer() {
        let cell = OnceCell::new();
        let mut stuck = Polled::new(cell.get_or_init(core::future::pending));
        stuck.pending();
        let mut waiting = Polled::new(cell.get_or_init(|| async { 2 }));
        waiting.pending();
        assert_eq!(*cell.get_or_init_racy(|| 1), 1);
        assert_eq!(waiting.wakes(), 1, "the publisher wakes the queue");
        assert_eq!(*waiting.ready(), 1);
        let mut late = Polled::new(cell.get_or_init(|| async { 3 }));
        assert_eq!(*late.ready(), 1);
        assert_eq!(
            cell.set(9),
            Err(SetError::AlreadyInitializedError(9)),
            "the cell is full, not being initialized"
        );
        drop(stuck);
    }

    #[test]
    fn an_initializer_that_lost_to_a_racing_builder_drops_its_value() {
        let cell = OnceCell::new();
        let opened = Event::new();
        let mut slow = Polled::new(cell.get_or_init(|| async {
            opened.wait().await;
            Arc::new(5)
        }));
        slow.pending();
        let racy = Arc::new(7);
        assert_eq!(**cell.get_or_init_racy(|| Arc::clone(&racy)), 7);
        opened.set();
        assert_eq!(**slow.ready(), 7, "the racer's value won");
        assert_eq!(Arc::strong_count(&racy), 2, "the cell holds the winner");
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
        let mut first = Polled::new(cell.get_or_init(|| async {
            opened.wait().await;
            1
        }));
        first.pending();
        let mut second = Polled::new(cell.get_or_init(|| async { 2 }));
        second.pending();
        opened.set();
        assert_eq!(*first.ready(), 1);
        assert_eq!(second.wakes(), 1);
        assert_eq!(*second.ready(), 1);
    }

    #[test]
    fn a_closed_turn_queue_over_an_empty_cell_still_answers() {
        // Nothing in this module closes the queue of an empty cell; if one
        // were, the caller builds the value itself.
        let cell = OnceCell::<u8>::new();
        cell.init.close();
        assert_eq!(*block_on(cell.get_or_init(|| async { 5 })), 5);
        assert_eq!(cell.get(), Some(&5));
        let failed = OnceCell::<u8>::new();
        failed.init.close();
        assert_eq!(
            block_on(failed.get_or_try_init(|| async { Err::<u8, &str>("no") })),
            Err("no")
        );
        assert_eq!(failed.get(), None);
    }

    #[test]
    fn the_initializer_is_not_called_when_the_cell_is_full() {
        let cell = OnceCell::from(1);
        let mut called = false;
        let value = block_on(cell.get_or_init(|| {
            called = true;
            async { 2 }
        }));
        assert_eq!(*value, 1);
        assert!(!called);
        let tried = block_on(cell.get_or_try_init(|| async { Err::<u8, ()>(()) }));
        assert_eq!(tried, Ok(&1));
    }

    #[test]
    fn a_dropped_initializer_passes_the_turn_on() {
        let cell = OnceCell::new();
        let mut first = Polled::new(cell.get_or_init(core::future::pending));
        first.pending();
        let mut second = Polled::new(cell.get_or_init(|| async { 2 }));
        second.pending();
        drop(first);
        assert_eq!(second.wakes(), 1);
        assert_eq!(*second.ready(), 2);
    }

    #[test]
    fn a_failed_initializer_lets_the_next_one_try() {
        let cell = OnceCell::new();
        let opened = Event::new();
        let mut failing = Polled::new(cell.get_or_try_init(|| async {
            opened.wait().await;
            Err::<u32, &str>("no")
        }));
        failing.pending();
        let mut next = Polled::new(cell.get_or_try_init(|| async { Ok::<u32, &str>(3) }));
        next.pending();
        opened.set();
        assert_eq!(failing.ready(), Err("no"));
        assert_eq!(next.ready(), Ok(&3));
    }

    #[test]
    fn constructors_conversions_and_traits() {
        static FULL: OnceCell<u32> = OnceCell::const_new_with(4);
        static EMPTY: OnceCell<u32> = OnceCell::const_new();
        assert_eq!(FULL.get(), Some(&4));
        assert_eq!(EMPTY.get(), None);
        assert_eq!(FULL.set(5), Err(SetError::AlreadyInitializedError(5)));
        assert_eq!(*block_on(FULL.get_or_init(|| async { 6 })), 4);
        assert_eq!(OnceCell::new_with(Some(1)).get(), Some(&1));
        assert_eq!(OnceCell::<u8>::new_with(None).get(), None);
        assert_eq!(OnceCell::<u8>::default().get(), None);
        assert_eq!(OnceCell::from(2).into_inner(), Some(2));
        assert_eq!(
            format!("{:?}", OnceCell::from(2)),
            "OnceCell { value: Some(2) }"
        );
        assert_eq!(
            format!("{:?}", OnceCell::<u8>::new()),
            "OnceCell { value: None }"
        );

        let cell = OnceCell::from(String::from("a"));
        let copy = cell.clone();
        assert_eq!(copy, cell);
        assert_eq!(copy.get().map(String::as_str), Some("a"));
        assert_ne!(cell, OnceCell::new());
        assert_eq!(OnceCell::<u8>::new(), OnceCell::new());
        assert_eq!(OnceCell::<String>::new().clone().get(), None);
    }

    #[test]
    fn get_mut_and_take_work_for_either_home_of_the_value() {
        let mut inline = OnceCell::new();
        assert_eq!(inline.get_mut(), None);
        assert_eq!(inline.set(vec![1]), Ok(()));
        inline.get_mut().expect("full").push(2);
        assert_eq!(inline.take(), Some(vec![1, 2]));
        assert_eq!(inline.get(), None);
        assert_eq!(inline.set(vec![3]), Ok(()), "a taken cell is set again");

        let mut boxed = OnceCell::new();
        assert_eq!(*boxed.get_or_init_racy(|| vec![1]), vec![1]);
        boxed.get_mut().expect("full").push(2);
        assert_eq!(boxed.take(), Some(vec![1, 2]));
        assert_eq!(boxed.take(), None);
        assert_eq!(*boxed.get_or_init_racy(|| vec![9]), vec![9]);

        let mut const_full = OnceCell::const_new_with(String::from("c"));
        const_full.get_mut().expect("full").push('d');
        assert_eq!(const_full.into_inner().as_deref(), Some("cd"));
    }

    #[test]
    fn every_value_is_dropped_exactly_once() {
        let marker = Arc::new(());
        let racy = OnceCell::new();
        racy.get_or_init_racy(|| Arc::clone(&marker));
        let inline = OnceCell::new();
        assert!(inline.set(Arc::clone(&marker)).is_ok());
        let lost = OnceCell::new();
        assert!(lost.set(Arc::clone(&marker)).is_ok());
        assert!(lost.set(Arc::clone(&marker)).is_err());
        let from = OnceCell::from(Arc::clone(&marker));
        assert_eq!(Arc::strong_count(&marker), 5);
        drop((racy, inline, lost, from));
        assert_eq!(Arc::strong_count(&marker), 1);
        let zst = OnceCell::new();
        assert!(zst.set(()).is_ok());
        assert_eq!(zst.get(), Some(&()));
        let zst_racy = OnceCell::<()>::new();
        assert_eq!(zst_racy.get_or_init_racy(|| ()), &());
    }

    #[test]
    fn threads_setting_and_initializing_agree_on_one_value() {
        let rounds = if cfg!(miri) { 3 } else { 200 };
        for _ in 0..rounds {
            let cell = Arc::new(OnceCell::new());
            let setter = {
                let cell = Arc::clone(&cell);
                std::thread::spawn(move || cell.set(1).is_ok())
            };
            let init = {
                let cell = Arc::clone(&cell);
                std::thread::spawn(move || *block_on(cell.get_or_init(|| async { 2 })))
            };
            let set_won = setter.join().expect("setter");
            let got = init.join().expect("init");
            assert_eq!(got, *cell.get().expect("full"));
            if set_won {
                assert_eq!(got, 1);
            }
        }
    }
}
