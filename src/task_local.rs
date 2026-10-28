//! Task-local values: [`task_local!`](crate::task_local) declares a key, and
//! [`LocalKey::scope`] runs a future with a value set for that key in every
//! one of its polls, wherever the task runs. The value lives in the future,
//! not in the thread: it is swapped into the key's thread-local slot for each
//! poll and taken back after it, so tasks of one shard never see each other's
//! values.

use core::cell::RefCell;
use core::fmt;
use core::future::Future;
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Declares task-local keys, each a [`LocalKey`].
///
/// ```
/// nuclei::task_local! {
///     static REQUEST_ID: u64;
/// }
///
/// # nuclei::block_on(async {
/// REQUEST_ID
///     .scope(7, async {
///         assert_eq!(REQUEST_ID.get(), 7);
///     })
///     .await;
/// # });
/// ```
#[macro_export]
macro_rules! task_local {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty; $($rest:tt)*) => {
        $crate::__task_local_key!($(#[$attr])* $vis $name, $t);
        $crate::task_local!($($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty) => {
        $crate::__task_local_key!($(#[$attr])* $vis $name, $t);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __task_local_key {
    ($(#[$attr:meta])* $vis:vis $name:ident, $t:ty) => {
        $(#[$attr])*
        $vis static $name: $crate::task::LocalKey<$t> = {
            ::std::thread_local! {
                static __KEY: ::core::cell::RefCell<::core::option::Option<$t>> =
                    const { ::core::cell::RefCell::new(::core::option::Option::None) };
            }
            $crate::task::LocalKey { inner: __KEY }
        };
    };
}

/// A task-local key, declared with [`task_local!`](crate::task_local).
pub struct LocalKey<T: 'static> {
    #[doc(hidden)]
    pub inner: std::thread::LocalKey<RefCell<Option<T>>>,
}

/// The key was read where no scope set it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessError {
    _private: (),
}

impl fmt::Display for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("task-local value not set")
    }
}

impl std::error::Error for AccessError {}

impl<T: 'static> LocalKey<T> {
    /// A future that runs `future` with the key set to `value` in each of its
    /// polls, and drops `future` (when it is dropped early) with the value
    /// still set.
    pub fn scope<F: Future>(&'static self, value: T, future: F) -> TaskLocalFuture<T, F> {
        TaskLocalFuture {
            key: self,
            slot: Some(value),
            future: Some(future),
            _pinned: PhantomPinned,
        }
    }

    /// Runs `f` with the key set to `value`, then unsets it.
    pub fn sync_scope<F: FnOnce() -> R, R>(&'static self, value: T, f: F) -> R {
        let mut slot = Some(value);
        self.enter(&mut slot, f)
    }

    /// Runs `f` on the key's value.
    ///
    /// # Panics
    ///
    /// Where no scope set the key; [`try_with`](Self::try_with) answers an
    /// [`AccessError`] instead.
    #[expect(
        clippy::panic,
        reason = "documented under `# Panics`; `try_with` answers the error instead"
    )]
    pub fn with<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> R {
        match self.try_with(f) {
            Ok(r) => r,
            Err(e) => panic!("{e}: no `scope` set this task-local key"),
        }
    }

    /// Runs `f` on the key's value, or answers an [`AccessError`] where no
    /// scope set it.
    pub fn try_with<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> Result<R, AccessError> {
        self.inner
            .try_with(|cell| cell.borrow().as_ref().map(f))
            .ok()
            .flatten()
            .ok_or(AccessError { _private: () })
    }

    /// A clone of the key's value.
    ///
    /// # Panics
    ///
    /// Where no scope set the key.
    pub fn get(&'static self) -> T
    where
        T: Clone,
    {
        self.with(T::clone)
    }

    /// Moves `slot`'s value into the key for `f`, and back out after it, even
    /// when `f` panics: an outer scope's value is put back too.
    #[expect(
        clippy::panic,
        reason = "a key used during this thread's teardown, where no value can be swapped in"
    )]
    fn enter<F: FnOnce() -> R, R>(&'static self, slot: &mut Option<T>, f: F) -> R {
        struct Restore<'a, T: 'static> {
            key: &'static LocalKey<T>,
            slot: &'a mut Option<T>,
        }
        impl<T: 'static> Drop for Restore<'_, T> {
            fn drop(&mut self) {
                let _ = self.key.inner.try_with(|cell| {
                    core::mem::swap(&mut *cell.borrow_mut(), self.slot);
                });
            }
        }
        let swapped = self
            .inner
            .try_with(|cell| core::mem::swap(&mut *cell.borrow_mut(), slot));
        if swapped.is_err() {
            panic!("the task-local key is used while this thread's locals are torn down");
        }
        let _restore = Restore { key: self, slot };
        f()
    }
}

impl<T: 'static> fmt::Debug for LocalKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalKey").finish_non_exhaustive()
    }
}

/// The future of [`LocalKey::scope`].
pub struct TaskLocalFuture<T: 'static, F> {
    key: &'static LocalKey<T>,
    /// The value while the future is not being polled.
    slot: Option<T>,
    /// The scoped future, until it completes or is dropped.
    future: Option<F>,
    _pinned: PhantomPinned,
}

impl<T: 'static, F: Future> Future for TaskLocalFuture<T, F> {
    type Output = F::Output;

    #[expect(
        clippy::panic,
        reason = "polled after it completed, which the `Future` contract rules out"
    )]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: `future` is never moved out while pinned (it is dropped in
        // place, by `Option::take` only after it completed or in `Drop`), and
        // `key`, `slot` are not structurally pinned.
        let this = unsafe { self.get_unchecked_mut() };
        let key = this.key;
        let slot = &mut this.slot;
        let future = &mut this.future;
        key.enter(slot, || {
            let Some(inner) = future.as_mut() else {
                panic!("`TaskLocalFuture` polled after it completed");
            };
            // SAFETY: see above; the inner future stays where it is.
            let poll = unsafe { Pin::new_unchecked(inner) }.poll(cx);
            if poll.is_ready() {
                *future = None;
            }
            poll
        })
    }
}

impl<T: 'static, F> Drop for TaskLocalFuture<T, F> {
    fn drop(&mut self) {
        if self.future.is_some() {
            let future = &mut self.future;
            // The scoped future's drop sees the value, as its polls did; a key
            // that cannot be entered any more (locals torn down) drops it
            // without.
            let key = self.key;
            let entered = key.inner.try_with(|_| ()).is_ok();
            if entered {
                key.enter(&mut self.slot, || *future = None);
            } else {
                *future = None;
            }
        }
    }
}

impl<T: 'static + fmt::Debug, F> fmt::Debug for TaskLocalFuture<T, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskLocalFuture")
            .field("value", &self.slot)
            .finish_non_exhaustive()
    }
}
