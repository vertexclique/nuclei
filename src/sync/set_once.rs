//! A cell set once, whose readers can wait for the value.

use core::error::Error;
use core::fmt;

use super::event::Event;
use super::once_cell::{OnceCell, SetError};

/// Error returned from [`SetOnce::set`]: the cell was set already, or
/// another `set` is setting it now. The value comes back.
#[derive(Debug, PartialEq, Eq)]
pub struct SetOnceError<T>(pub T);

impl<T> fmt::Display for SetOnceError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SetOnceError")
    }
}

impl<T: fmt::Debug> Error for SetOnceError<T> {}

/// A cell that is set once, and that readers can wait on until it is.
///
/// It is a [`OnceCell`] that nobody initializes asynchronously, paired with
/// an [`Event`] that [`wait`](Self::wait) sleeps on: [`set`](Self::set)
/// publishes the value with one compare-and-swap, and then sets the event,
/// which wakes every waiter.
///
/// ```
/// use nuclei::sync::SetOnce;
///
/// static GREETING: SetOnce<&str> = SetOnce::const_new();
/// assert!(GREETING.get().is_none());
/// assert!(GREETING.set("hello").is_ok());
/// assert_eq!(GREETING.get(), Some(&"hello"));
/// assert!(GREETING.set("again").is_err());
/// ```
pub struct SetOnce<T> {
    cell: OnceCell<T>,
    ready: Event,
}

impl<T> SetOnce<T> {
    loom_const_fn! {
        /// An unset cell.
        pub fn new() -> Self {
            Self { cell: OnceCell::new(), ready: Event::new() }
        }
    }

    loom_const_fn! {
        /// An unset cell, usable in a `static`.
        pub fn const_new() -> Self {
            Self::new()
        }
    }

    /// A cell that holds `value` if there is one, and is unset otherwise.
    pub fn new_with(value: Option<T>) -> Self {
        match value {
            Some(value) => Self::from(value),
            None => Self::new(),
        }
    }

    loom_const_fn! {
        /// A cell that is set to `value` already, usable in a `static`.
        pub fn const_new_with(value: T) -> Self {
            Self { cell: OnceCell::const_new_with(value), ready: Event::new_set(true) }
        }
    }

    /// Whether the cell holds a value.
    pub fn initialized(&self) -> bool {
        self.cell.initialized()
    }

    /// The value, once set.
    pub fn get(&self) -> Option<&T> {
        self.cell.get()
    }

    /// Sets the value, and wakes every task waiting in
    /// [`wait`](Self::wait). One compare-and-swap publishes; never waits.
    ///
    /// # Errors
    ///
    /// [`SetOnceError`] with the value when the cell is set already, or
    /// another call is setting it at this moment.
    pub fn set(&self, value: T) -> Result<(), SetOnceError<T>> {
        match self.cell.set(value) {
            Ok(()) => {
                self.ready.set();
                Ok(())
            }
            Err(SetError::AlreadyInitializedError(value) | SetError::InitializingError(value)) => {
                Err(SetOnceError(value))
            }
        }
    }

    /// Consumes the cell and returns its value, if it has one.
    pub fn into_inner(self) -> Option<T> {
        self.cell.into_inner()
    }

    /// Waits until the cell is set, and returns the value: at once if it
    /// is set already.
    ///
    /// Dropping the future withdraws the wait.
    pub async fn wait(&self) -> &T {
        loop {
            if let Some(value) = self.cell.get() {
                return value;
            }
            self.ready.wait().await;
        }
    }
}

impl<T> Default for SetOnce<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: fmt::Debug> fmt::Debug for SetOnce<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetOnce")
            .field("value", &self.get())
            .finish()
    }
}

impl<T: Clone> Clone for SetOnce<T> {
    fn clone(&self) -> Self {
        Self::new_with(self.get().cloned())
    }
}

impl<T: PartialEq> PartialEq for SetOnce<T> {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl<T: Eq> Eq for SetOnce<T> {}

impl<T> From<T> for SetOnce<T> {
    fn from(value: T) -> Self {
        Self::const_new_with(value)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::{Polled, block_on};
    use std::sync::Arc;

    #[test]
    fn a_set_wakes_every_waiter_and_later_waits_pass() {
        let once = SetOnce::new();
        let mut first = Polled::new(once.wait());
        first.pending();
        let mut second = Polled::new(once.wait());
        second.pending();
        assert!(!once.initialized());
        assert_eq!(once.set(7), Ok(()));
        assert!(once.initialized());
        assert_eq!((first.wakes(), second.wakes()), (1, 1));
        assert_eq!((first.ready(), second.ready()), (&7, &7));
        assert_eq!(Polled::new(once.wait()).ready(), &7);
    }

    #[test]
    fn a_second_set_gives_its_value_back() {
        let once = SetOnce::new();
        assert_eq!(once.set(1), Ok(()));
        let err = once.set(2).unwrap_err();
        assert_eq!(err, SetOnceError(2));
        assert_eq!(err.0, 2);
        assert_eq!(err.to_string(), "SetOnceError");
        assert_eq!(once.get(), Some(&1));
        assert_eq!(once.into_inner(), Some(1));
    }

    #[test]
    fn a_dropped_wait_leaves_the_rest_to_their_wake() {
        let once = SetOnce::new();
        let mut kept = Polled::new(once.wait());
        kept.pending();
        Polled::new(once.wait()).pending();
        assert_eq!(once.set(5), Ok(()));
        assert_eq!(kept.ready(), &5);
    }

    #[test]
    fn constructors_conversions_and_traits() {
        static FULL: SetOnce<u8> = SetOnce::const_new_with(3);
        static EMPTY: SetOnce<u8> = SetOnce::const_new();
        assert_eq!(FULL.get(), Some(&3));
        assert!(FULL.set(4).is_err());
        assert_eq!(*block_on(FULL.wait()), 3);
        assert_eq!(EMPTY.get(), None);
        assert_eq!(SetOnce::new_with(Some(1)).get(), Some(&1));
        assert_eq!(SetOnce::<u8>::new_with(None).get(), None);
        assert_eq!(SetOnce::<u8>::default().into_inner(), None);
        assert_eq!(
            format!("{:?}", SetOnce::from(2)),
            "SetOnce { value: Some(2) }"
        );
        let once = SetOnce::from(String::from("x"));
        assert_eq!(once.clone(), once);
        assert_ne!(once, SetOnce::new());
    }

    #[test]
    fn a_thread_waiting_for_the_value_gets_it() {
        let once = Arc::new(SetOnce::new());
        let waiter = {
            let once = Arc::clone(&once);
            std::thread::spawn(move || *block_on(once.wait()))
        };
        assert_eq!(once.set(9), Ok(()));
        assert_eq!(waiter.join().expect("waiter"), 9);
    }

    #[test]
    fn two_sets_at_once_leave_exactly_one_winner() {
        for _ in 0..if cfg!(miri) { 3 } else { 200 } {
            let once = Arc::new(SetOnce::new());
            let racers: Vec<_> = [1, 2]
                .into_iter()
                .map(|n| {
                    let once = Arc::clone(&once);
                    std::thread::spawn(move || once.set(n).is_ok())
                })
                .collect();
            let wins = racers
                .into_iter()
                .map(|racer| racer.join().expect("racer"))
                .filter(|won| *won)
                .count();
            assert_eq!(wins, 1);
            assert!(once.get().is_some());
        }
    }
}
