//! A multi-producer, multi-consumer channel that retains only the *last* sent
//! value.
//!
//! [`channel`] returns a [`Sender`] and a [`Receiver`] around an initial
//! value. Each [`Receiver`] tracks the last value *seen* by its own caller.
//! The API is the established async watch-channel API; nothing here needs a
//! runtime.
//!
//! - [`Receiver::borrow_and_update`] returns the current value and marks it
//!   seen. [`Receiver::borrow`] returns it without marking it seen.
//! - [`Receiver::changed`] completes when a value the receiver has not seen
//!   is in the channel, and marks it seen. At creation the initial value is
//!   seen, and a receiver made by [`Sender::subscribe`] sees the value that
//!   was current at that moment as seen.
//! - [`Receiver::has_changed`] is the synchronous check; it errors if and
//!   only if the channel is closed, while `changed` errors only when the
//!   channel is closed *and* the current value is seen.
//! - [`Sender::is_closed`] and [`Sender::closed`] tell the producer that
//!   every [`Receiver`] is gone. The value is dropped when every sender and
//!   every receiver is gone.
//!
//! ```
//! use nuclei::sync::watch;
//!
//! let (tx, mut rx) = watch::channel("hello");
//! assert_eq!(*rx.borrow_and_update(), "hello");
//! assert!(!rx.has_changed().unwrap());
//!
//! tx.send("world").unwrap();
//! assert!(rx.has_changed().unwrap());
//! assert_eq!(*rx.borrow_and_update(), "world");
//! ```
//!
//! # The value lives in an `Atom`
//!
//! Where a lock-based channel guards the value with a read-write lock, this
//! one keeps it in an [`Atom`]. A send publishes a new allocation with
//! one swap; a borrow is one protected load and never blocks a sender, and a
//! sender never blocks a borrow. Two consequences follow, and both differ
//! from the lock-based API:
//!
//! - The channel needs `T: Send + Sync + 'static`: values are shared by
//!   reference between threads and reclaimed later.
//! - A [`Ref`] does not hold a lock but a reclamation guard. A value that was
//!   replaced stays allocated until every `Ref` that can see it is dropped,
//!   so keep a `Ref` short. It is `!Send`: it must be dropped on the thread
//!   that took it, and the compiler therefore refuses to hold one across an
//!   await in a task that may move threads.
//! - The old value cannot be handed back by value while readers may still
//!   look at it, so [`Sender::send_replace`] returns a *clone* (`T: Clone`),
//!   and the two in-place modifiers, [`Sender::send_modify`] and
//!   [`Sender::send_if_modified`], work on a clone too: they copy the value,
//!   let the closure change the copy and publish it with a
//!   compare-and-swap, retrying on a racing sender. Their closures are
//!   `FnMut` and may run more than once. A closure that panics, or that
//!   returns `false` from `send_if_modified`, publishes nothing, and the
//!   channel keeps the value it had.
//!
//! # Progress
//!
//! - Wait-free (a bounded number of the caller's own steps whatever other
//!   threads do): [`Receiver::borrow`], [`Receiver::borrow_and_update`],
//!   [`Receiver::has_changed`], [`Receiver::is_closed`],
//!   [`Receiver::mark_changed`], [`Receiver::mark_unchanged`],
//!   [`Sender::borrow`], [`Sender::is_closed`], [`Sender::subscribe`],
//!   [`Sender::receiver_count`], [`Sender::sender_count`], both `same_channel`
//!   checks, the clones, and dropping a [`Ref`]. A borrow is an `Atom`
//!   load, which is wait-free.
//! - The *publication* of [`Sender::send`] and [`Sender::send_replace`] (the
//!   swap and the version bump) is wait-free, but each ends by waking the
//!   waiting receivers through [`Notify::notify_waiters`](super::Notify),
//!   which is lock-free: it takes the queue's drain role with a
//!   compare-and-swap, or leaves a mark for the thread that holds it, and a
//!   pass may serve waiters other threads queue meanwhile. So `send` and
//!   `send_replace` are lock-free, not wait-free. So is dropping the last
//!   sender or the last receiver, for the same wake.
//! - [`Sender::send_modify`] and [`Sender::send_if_modified`] are lock-free:
//!   they retry the compare-and-swap whenever another sender published
//!   first, so a call can be delayed without bound by other senders' progress,
//!   and each retry runs the closure again.
//! - Polling [`Receiver::changed`], [`Receiver::wait_for`] and
//!   [`Sender::closed`] is lock-free: a poll may queue a waiter, which is a
//!   lock-free queue operation.

#![allow(unsafe_code)]

use core::fmt;
use core::ops::Deref;

use kovan::{Atom, AtomGuard, CachePadded};

use super::Notify;
use super::internal::{Arc, AtomicU64, AtomicUsize, Ordering};

use self::error::{RecvError, SendError};

/// The low bit of the state word: every sender is gone.
const CLOSED: u64 = 1;
/// One send. Two keeps [`CLOSED`] out of the version.
const STEP: u64 = 2;

/// The version part of a state word.
fn version_of(state: u64) -> u64 {
    state & !CLOSED
}

pub mod error {
    //! Error types of the watch channel.

    use core::fmt;
    use std::error::Error;

    /// Error produced when sending a value fails: every receiver is gone.
    ///
    /// The value is handed back in the error.
    #[derive(PartialEq, Eq, Clone, Copy)]
    pub struct SendError<T>(pub T);

    impl<T> fmt::Debug for SendError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("SendError").finish_non_exhaustive()
        }
    }

    impl<T> fmt::Display for SendError<T> {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(fmt, "channel closed")
        }
    }

    impl<T> Error for SendError<T> {}

    /// Error produced when receiving a change notification: every sender is
    /// gone.
    #[derive(Debug, Clone)]
    pub struct RecvError(pub(super) ());

    impl fmt::Display for RecvError {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(fmt, "channel closed")
        }
    }

    impl Error for RecvError {}
}

struct Shared<T: Send + Sync + 'static> {
    /// The most recent value.
    value: Atom<T>,
    /// The version in the high bits, [`CLOSED`] in the low bit. The version
    /// moves after the value it counts is published and never before, which
    /// is what lets a reader pair a version with a value that is at least as
    /// new. Padded: every send writes it and every borrow reads it.
    state: CachePadded<AtomicU64>,
    /// How many [`Receiver`]s exist.
    ref_count_rx: AtomicUsize,
    /// How many [`Sender`]s exist.
    ref_count_tx: AtomicUsize,
    /// Wakes receivers waiting for a change.
    notify_rx: Notify,
    /// Wakes the producer waiting for the receivers to go.
    notify_tx: Notify,
}

impl<T: Send + Sync + 'static> Shared<T> {
    /// A value was just published: count it, then wake the receivers.
    ///
    /// The order matters twice. The version moves after the publication so a
    /// reader that sees the version also finds that value or a newer one
    /// (see [`Self::snapshot`]), and the wake comes after the version so a
    /// receiver that was woken finds the new version when it looks (see
    /// `changed_impl`).
    fn published(&self) {
        // Release: pairs with the Acquire loads of the state, publishing the
        // value written before it.
        self.state.fetch_add(STEP, Ordering::Release);
        self.notify_rx.notify_waiters();
    }

    /// A value and a version that is no newer than it.
    ///
    /// The version is read first. It is bumped only after its value is
    /// published, so the Acquire load of the state makes every publication
    /// the version counts happen before the load of the value, and the value
    /// is the newest of those or something newer. The reverse order could
    /// pair a stale value with a version that already counts a newer one and
    /// let a receiver mark that newer version seen without ever reading it.
    /// The cost of this order is that a receiver can read a value one send
    /// ahead of its version: a harmless extra `changed`, which the docs of
    /// `Receiver::borrow` warn of.
    fn snapshot(&self) -> (AtomGuard<'_, T>, u64) {
        let version = version_of(self.state.load(Ordering::Acquire));
        (self.value.load(), version)
    }
}

impl<T: Send + Sync + 'static + fmt::Debug> fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.load(Ordering::Acquire);
        f.debug_struct("Shared")
            .field("value", &*self.value.load())
            .field("version", &version_of(state))
            .field("is_closed", &(state & CLOSED != 0))
            .field("ref_count_rx", &self.ref_count_rx)
            .finish()
    }
}

/// Creates a new watch channel around `init`, returning the send and receive
/// handles.
///
/// All values sent by the [`Sender`] become visible to the [`Receiver`]
/// handles; only the last value sent is kept, and the ones before it are
/// dropped. The initial value counts as seen by the first receiver.
///
/// ```
/// use nuclei::sync::watch;
///
/// let (tx, rx) = watch::channel(0u32);
/// tx.send(1).unwrap();
/// assert_eq!(*rx.borrow(), 1);
/// ```
pub fn channel<T: Send + Sync + 'static>(init: T) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        value: Atom::new(init),
        state: CachePadded::new(AtomicU64::new(0)),
        ref_count_rx: AtomicUsize::new(1),
        ref_count_tx: AtomicUsize::new(1),
        notify_rx: Notify::new(),
        notify_tx: Notify::new(),
    });
    let tx = Sender {
        shared: Arc::clone(&shared),
    };
    let rx = Receiver { shared, version: 0 };
    (tx, rx)
}

/// Sends values to the associated [`Receiver`]s.
///
/// Created by [`channel`]. Clone it for more producers. The channel closes
/// for the receivers when the last sender is dropped.
///
/// Requires `T: Send + Sync + 'static`, see the [module docs](self).
pub struct Sender<T: Send + Sync + 'static> {
    shared: Arc<Shared<T>>,
}

/// Receives values from the associated [`Sender`]s.
///
/// Created by [`channel`] or [`Sender::subscribe`]; clone it for more
/// consumers, each with its own record of what it has seen.
///
/// Requires `T: Send + Sync + 'static`, see the [module docs](self).
pub struct Receiver<T: Send + Sync + 'static> {
    shared: Arc<Shared<T>>,
    /// The last version this receiver saw.
    version: u64,
}

/// A reference to the value inside the channel.
///
/// Unlike a lock guard, a `Ref` never blocks a sender. It keeps the value it
/// points to, and every value replaced since, allocated until it is dropped,
/// so keep it short. It must be dropped on the thread that took it: it is
/// not `Send`.
pub struct Ref<'a, T: Send + Sync + 'static> {
    guard: AtomGuard<'a, T>,
    has_changed: bool,
}

// SAFETY: a shared `&Ref` gives `&T` and a bool. `T: Sync` makes the `&T`
// safe on any thread, and the reclamation guard inside stays pinned on the
// thread that owns the `Ref` for as long as the `Ref` lives, so the value
// outlives every reference a `&Ref` hands out.
unsafe impl<T: Send + Sync + 'static> Sync for Ref<'_, T> {}

impl<T: Send + Sync + 'static> Ref<'_, T> {
    /// Whether the borrowed value is considered *changed* since it was last
    /// marked seen.
    ///
    /// Unlike [`Receiver::has_changed`], this does not fail when the channel
    /// is closed. For a borrow taken from the [`Sender`] it is always
    /// `false`.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, mut rx) = watch::channel("hello");
    /// tx.send("goodbye").unwrap();
    /// assert!(!tx.borrow().has_changed());
    /// drop(tx);
    /// assert!(rx.borrow().has_changed());
    /// assert!(rx.borrow_and_update().has_changed());
    /// assert!(!rx.borrow().has_changed());
    /// ```
    pub fn has_changed(&self) -> bool {
        self.has_changed
    }
}

impl<T: Send + Sync + 'static> Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T: Send + Sync + 'static + fmt::Debug> fmt::Debug for Ref<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ref")
            .field("value", &**self)
            .field("has_changed", &self.has_changed)
            .finish()
    }
}

impl<T: Send + Sync + 'static> Receiver<T> {
    fn from_shared(version: u64, shared: Arc<Shared<T>>) -> Self {
        // Relaxed: a plain counter; no memory is published through it.
        shared.ref_count_rx.fetch_add(1, Ordering::Relaxed);
        Self { shared, version }
    }

    /// Returns a reference to the most recently sent value.
    ///
    /// Does not mark the value seen, so a later [`changed`](Self::changed)
    /// can return at once even though you have read it. If you wait for
    /// changes in a loop, prefer [`borrow_and_update`](Self::borrow_and_update).
    ///
    /// Wait-free, and it never blocks a sender. A borrow keeps the value it
    /// points to alive; keep it short and drop it before awaiting.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (_tx, rx) = watch::channel("hello");
    /// assert_eq!(*rx.borrow(), "hello");
    /// ```
    pub fn borrow(&self) -> Ref<'_, T> {
        let (guard, version) = self.shared.snapshot();
        Ref {
            guard,
            has_changed: self.version != version,
        }
    }

    /// Returns a reference to the most recently sent value and marks it seen.
    ///
    /// Later calls to [`changed`](Self::changed) do not return until a value
    /// newer than this one is sent. Wait-free; see [`borrow`](Self::borrow)
    /// for the advice on holding the reference.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, mut rx) = watch::channel("hello");
    /// tx.send("goodbye").unwrap();
    /// assert_eq!(*rx.borrow_and_update(), "goodbye");
    /// assert!(!rx.has_changed().unwrap());
    /// ```
    pub fn borrow_and_update(&mut self) -> Ref<'_, T> {
        let (guard, version) = self.shared.snapshot();
        let has_changed = self.version != version;
        self.version = version;
        Ref { guard, has_changed }
    }

    /// Checks whether the channel holds a value this receiver has not seen.
    /// The value is not marked seen.
    ///
    /// It does not compare values: the same value sent twice counts as a
    /// change.
    ///
    /// # Errors
    ///
    /// Returns a [`RecvError`] if and only if the channel is closed.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, mut rx) = watch::channel("hello");
    /// tx.send("goodbye").unwrap();
    /// assert!(rx.has_changed().unwrap());
    /// assert_eq!(*rx.borrow_and_update(), "goodbye");
    /// assert!(!rx.has_changed().unwrap());
    /// drop(tx);
    /// assert!(rx.has_changed().is_err());
    /// ```
    pub fn has_changed(&self) -> Result<bool, RecvError> {
        let state = self.shared.state.load(Ordering::Acquire);
        if state & CLOSED != 0 {
            return Err(RecvError(()));
        }
        Ok(self.version != version_of(state))
    }

    /// Whether the channel is closed: every [`Sender`] has been dropped.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, rx) = watch::channel(());
    /// assert!(!rx.is_closed());
    /// drop(tx);
    /// assert!(rx.is_closed());
    /// ```
    pub fn is_closed(&self) -> bool {
        self.shared.state.load(Ordering::Acquire) & CLOSED != 0
    }

    /// Marks the current value as not seen.
    ///
    /// Afterwards [`has_changed`](Self::has_changed) is `true` and
    /// [`changed`](Self::changed) returns at once, whether or not a new value
    /// is sent. Useful to trigger a first notification after subscribing.
    pub fn mark_changed(&mut self) {
        // Wrapping: it must agree with the wrapping `fetch_add` of the sender.
        self.version = self.version.wrapping_sub(STEP);
    }

    /// Marks the current value as seen, without reading it.
    pub fn mark_unchanged(&mut self) {
        self.version = version_of(self.shared.state.load(Ordering::Acquire));
    }

    /// Waits for a change notification, then marks the current value as seen.
    ///
    /// If the current value has not been seen yet, this marks it seen and
    /// returns at once. Otherwise it waits until a sender sends a new value
    /// or the last sender is dropped.
    ///
    /// # Errors
    ///
    /// Returns a [`RecvError`] if the channel is closed *and* the current
    /// value is seen.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe: if the future is dropped before it completes, no value
    /// was marked seen by it.
    ///
    /// ```
    /// use nuclei::sync::watch;
    /// use std::future::Future;
    /// use std::pin::pin;
    /// use std::task::{Context, Poll, Waker};
    ///
    /// let (tx, mut rx) = watch::channel("hello");
    /// let mut cx = Context::from_waker(Waker::noop());
    /// let mut changed = pin!(rx.changed());
    /// assert!(changed.as_mut().poll(&mut cx).is_pending());
    /// tx.send("goodbye").unwrap();
    /// assert!(matches!(changed.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    /// ```
    pub async fn changed(&mut self) -> Result<(), RecvError> {
        changed_impl(&self.shared, &mut self.version).await
    }

    /// Waits for a value that satisfies `f`.
    ///
    /// Calls `f` on the current value first, even if it is already seen, and
    /// then on the newest value whenever something is sent. When `f` returns
    /// `true`, returns a reference to that value and marks it seen. The
    /// channel keeps only the latest value, so a burst of sends can skip
    /// values: `f` always sees the newest.
    ///
    /// # Errors
    ///
    /// Returns a [`RecvError`] if the channel is closed. Then `f` has been
    /// called on the last value and returned `false` for it.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe: the last value `f` returned `true` for is never marked
    /// seen by a dropped call.
    ///
    /// # Panics
    ///
    /// If and only if `f` panics. Nothing the receiver shares is poisoned.
    ///
    /// ```
    /// use nuclei::sync::watch;
    /// use std::future::Future;
    /// use std::pin::pin;
    /// use std::task::{Context, Poll, Waker};
    ///
    /// let (tx, mut rx) = watch::channel(0);
    /// let mut cx = Context::from_waker(Waker::noop());
    /// let mut wait = pin!(rx.wait_for(|v| *v == 2));
    /// assert!(wait.as_mut().poll(&mut cx).is_pending());
    /// tx.send(1).unwrap();
    /// assert!(wait.as_mut().poll(&mut cx).is_pending());
    /// tx.send(2).unwrap();
    /// match wait.as_mut().poll(&mut cx) {
    ///     Poll::Ready(Ok(value)) => assert_eq!(*value, 2),
    ///     _ => panic!("the value 2 was sent"),
    /// }
    /// ```
    pub async fn wait_for(
        &mut self,
        mut f: impl FnMut(&T) -> bool,
    ) -> Result<Ref<'_, T>, RecvError> {
        let mut closed = false;
        loop {
            // A value that fits at once spends one operation of the poll's
            // budget, as `changed` does.
            if !crate::coop::has_budget_remaining() {
                crate::task::yield_now().await;
            }
            {
                let (guard, new_version) = self.shared.snapshot();
                let has_changed = self.version != new_version;
                self.version = new_version;
                // After the channel closed, only a value that changed since
                // `f` last judged is worth another call.
                if (!closed || has_changed) && f(&guard) {
                    crate::coop::try_spend();
                    return Ok(Ref { guard, has_changed });
                }
            }
            if closed {
                return Err(RecvError(()));
            }
            closed = changed_impl(&self.shared, &mut self.version).await.is_err();
        }
    }

    /// Whether the two receivers belong to the same channel.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (_tx, rx) = watch::channel(true);
    /// let rx2 = rx.clone();
    /// assert!(rx.same_channel(&rx2));
    /// let (_tx3, rx3) = watch::channel(true);
    /// assert!(!rx3.same_channel(&rx2));
    /// ```
    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

/// The version after the one `version` records, or the end of the channel.
fn maybe_changed<T: Send + Sync + 'static>(
    shared: &Shared<T>,
    version: &mut u64,
) -> Option<Result<(), RecvError>> {
    let state = shared.state.load(Ordering::Acquire);
    let new_version = version_of(state);
    if *version != new_version {
        *version = new_version;
        return Some(Ok(()));
    }
    if state & CLOSED != 0 {
        return Some(Err(RecvError(())));
    }
    None
}

/// Waits until `maybe_changed` has an answer.
///
/// The wait cannot miss a send. The future for the next wake is created
/// first, and only then is the version read:
///
/// - A [`Notified`](super::Notified) hears every `notify_waiters` made after
///   it was *created*, polled or not.
/// - A sender bumps the version and only then calls `notify_waiters`.
///
/// So take a send. If its version bump is visible to the read below, the read
/// returns. If it is not, the bump came after the read, and `notify_waiters`,
/// which is after the bump, came after the creation of the `Notified` (the
/// read follows the creation in this thread's order, and a `notify_waiters`
/// seen by the creation would have made the bump visible through its Release
/// store), so the `Notified` completes. Either way the loop wakes up.
/// A completion without a new version is a spurious wake, and the loop
/// looks again.
async fn changed_impl<T: Send + Sync + 'static>(
    shared: &Shared<T>,
    version: &mut u64,
) -> Result<(), RecvError> {
    loop {
        // A change seen at once spends one operation of the poll's budget
        // (see `coop`): with the budget spent, the look waits for the next
        // poll, after the shard's other tasks.
        if !crate::coop::has_budget_remaining() {
            crate::task::yield_now().await;
        }
        let notified = shared.notify_rx.notified();
        if let Some(ret) = maybe_changed(shared, version) {
            crate::coop::try_spend();
            return ret;
        }
        notified.await;
    }
}

impl<T: Send + Sync + 'static> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self::from_shared(self.version, Arc::clone(&self.shared))
    }
}

impl<T: Send + Sync + 'static> Drop for Receiver<T> {
    fn drop(&mut self) {
        // Relaxed: the count publishes no memory. A producer waiting in
        // `closed` reads it after creating its `Notified`, which hears the
        // wake below, and that wake's Release makes this decrement visible.
        if self.shared.ref_count_rx.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.shared.notify_tx.notify_waiters();
        }
    }
}

impl<T: Send + Sync + 'static + fmt::Debug> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("shared", &*self.shared)
            .field("version", &self.version)
            .finish()
    }
}

impl<T: Send + Sync + 'static> Sender<T> {
    /// Creates the sending half of a channel with no receiver yet.
    ///
    /// Sending fails until [`subscribe`](Self::subscribe) creates one.
    ///
    /// ```
    /// use nuclei::sync::watch::Sender;
    ///
    /// let sender = Sender::new(0u8);
    /// assert!(sender.send(3).is_err());
    /// let _rx = sender.subscribe();
    /// assert!(sender.send(4).is_ok());
    /// ```
    pub fn new(init: T) -> Self {
        channel(init).0
    }

    /// Sends a new value, notifying all receivers.
    ///
    /// Fails if every receiver is gone; the value comes back in the error and
    /// is not kept for receivers that [`subscribe`](Self::subscribe) later.
    /// To always make a value available, even with no receiver, use
    /// [`send_replace`](Self::send_replace), [`send_modify`](Self::send_modify)
    /// or [`send_if_modified`](Self::send_if_modified).
    ///
    /// The publication is wait-free; the wake of the receivers makes the
    /// whole call lock-free (see the [module docs](self)).
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, rx) = watch::channel(1);
    /// tx.send(2).unwrap();
    /// assert_eq!(*rx.borrow(), 2);
    /// drop(rx);
    /// assert_eq!(tx.send(3).unwrap_err().0, 3);
    /// ```
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        // Only a hint: a receiver may come or go right after.
        if self.receiver_count() == 0 {
            return Err(SendError(value));
        }
        self.shared.value.store(value);
        self.shared.published();
        Ok(())
    }

    /// Modifies the watched value **unconditionally**, notifying all
    /// receivers.
    ///
    /// Also works with no receiver. `modify` runs on a clone of the current
    /// value, and the clone is published by a compare-and-swap; if another
    /// sender published first, `modify` runs again on the new value, so it is
    /// `FnMut`. Prefer [`send_if_modified`](Self::send_if_modified) when the
    /// value is only sometimes changed.
    ///
    /// Lock-free: it retries against other senders.
    ///
    /// # Panics
    ///
    /// Panics if `modify` (or the clone) panics. Nothing is published then
    /// and no receiver is notified: the channel keeps the value it had.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// #[derive(Clone)]
    /// struct State {
    ///     counter: usize,
    /// }
    /// let (tx, rx) = watch::channel(State { counter: 0 });
    /// tx.send_modify(|state| state.counter += 1);
    /// assert_eq!(rx.borrow().counter, 1);
    /// ```
    pub fn send_modify<F>(&self, mut modify: F)
    where
        F: FnMut(&mut T),
        T: Clone,
    {
        self.send_if_modified(|value| {
            modify(value);
            true
        });
    }

    /// Modifies the watched value **conditionally**, notifying all receivers
    /// only if it was modified.
    ///
    /// `modify` runs on a clone of the current value and returns `true` if
    /// it changed it. Only then is the clone published, by a
    /// compare-and-swap that starts over on the new value when another
    /// sender published first (so `modify` is `FnMut` and may run more than
    /// once). When it returns `false` nothing is published, whatever it did
    /// to its copy: unlike a lock-based channel there is no silent
    /// modification.
    ///
    /// Returns `true` if a new value was published and `false` if `modify`
    /// returned `false`.
    ///
    /// Lock-free: it retries against other senders.
    ///
    /// # Panics
    ///
    /// Panics if `modify` (or the clone) panics. Nothing is published then.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, mut rx) = watch::channel(1u32);
    /// let inc_if_odd = |n: &mut u32| {
    ///     if *n % 2 == 1 {
    ///         *n += 1;
    ///         return true;
    ///     }
    ///     false
    /// };
    /// assert!(tx.send_if_modified(inc_if_odd));
    /// assert_eq!(*rx.borrow_and_update(), 2);
    /// assert!(!tx.send_if_modified(inc_if_odd));
    /// assert!(!rx.has_changed().unwrap());
    /// ```
    pub fn send_if_modified<F>(&self, mut modify: F) -> bool
    where
        F: FnMut(&mut T) -> bool,
        T: Clone,
    {
        loop {
            let current = self.shared.value.load();
            let mut next = T::clone(&current);
            if !modify(&mut next) {
                return false;
            }
            // The guard in `current` keeps the compared value from being
            // freed and reused, so the pointer comparison is sound.
            if self.shared.value.compare_and_swap(&current, next).is_ok() {
                break;
            }
            // Another sender published first: start over from its value.
        }
        self.shared.published();
        true
    }

    /// Sends a new value, notifying all receivers, and returns a clone of the
    /// value it replaced.
    ///
    /// Also works with no receiver. The old value is returned as a clone
    /// because receivers may still be reading it; see the
    /// [module docs](self). The publication is wait-free; the wake of the
    /// receivers makes the whole call lock-free.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, _rx) = watch::channel(1);
    /// assert_eq!(tx.send_replace(2), 1);
    /// assert_eq!(tx.send_replace(3), 2);
    /// ```
    pub fn send_replace(&self, value: T) -> T
    where
        T: Clone,
    {
        let old = self.shared.value.swap(value);
        // Published before the clone: a clone that panics must not leave a
        // new value that no receiver hears about.
        self.shared.published();
        T::clone(&old)
    }

    /// Returns a reference to the most recently sent value.
    ///
    /// A borrow from the sender is never marked changed. See
    /// [`Receiver::borrow`] for the advice on holding the reference.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, _) = watch::channel("hello");
    /// assert_eq!(*tx.borrow(), "hello");
    /// ```
    pub fn borrow(&self) -> Ref<'_, T> {
        Ref {
            guard: self.shared.value.load(),
            has_changed: false,
        }
    }

    /// Whether the channel is closed for sending: every [`Receiver`] has been
    /// dropped. [`subscribe`](Self::subscribe) reopens it.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, rx) = watch::channel(());
    /// assert!(!tx.is_closed());
    /// drop(rx);
    /// assert!(tx.is_closed());
    /// ```
    pub fn is_closed(&self) -> bool {
        self.receiver_count() == 0
    }

    /// Completes when every receiver has been dropped.
    ///
    /// Lets the producer stop work nobody wants. If the channel closes
    /// briefly (the last receiver is dropped and [`subscribe`](Self::subscribe)
    /// is called again), this may or may not notice.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe.
    ///
    /// ```
    /// use nuclei::sync::watch;
    /// use std::future::Future;
    /// use std::pin::pin;
    /// use std::task::{Context, Poll, Waker};
    ///
    /// let (tx, rx) = watch::channel("hello");
    /// let mut cx = Context::from_waker(Waker::noop());
    /// let mut closed = pin!(tx.closed());
    /// assert_eq!(closed.as_mut().poll(&mut cx), Poll::Pending);
    /// drop(rx);
    /// assert_eq!(closed.as_mut().poll(&mut cx), Poll::Ready(()));
    /// ```
    pub async fn closed(&self) {
        while self.receiver_count() > 0 {
            // Created before the count is read again, for the reason given at
            // `changed_impl`: the last receiver's drop wakes it after the
            // decrement.
            let notified = self.shared.notify_tx.notified();
            if self.receiver_count() == 0 {
                return;
            }
            notified.await;
            // The channel may have been reopened by `subscribe`: look again.
        }
    }

    /// Creates a new [`Receiver`] connected to this sender.
    ///
    /// The value that is current now counts as seen by the new receiver. It
    /// works with no other receiver, which reopens the channel.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, _rx) = watch::channel(0u64);
    /// tx.send(5).unwrap();
    /// let rx = tx.subscribe();
    /// assert_eq!(*rx.borrow(), 5);
    /// assert!(!rx.has_changed().unwrap());
    /// ```
    pub fn subscribe(&self) -> Receiver<T> {
        // The CLOSED bit tracks only whether the senders are gone, so a
        // reopened channel needs no reset of it.
        let version = version_of(self.shared.state.load(Ordering::Acquire));
        Receiver::from_shared(version, Arc::clone(&self.shared))
    }

    /// How many receivers exist now.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, rx) = watch::channel("hello");
    /// assert_eq!(tx.receiver_count(), 1);
    /// let _rx2 = rx.clone();
    /// assert_eq!(tx.receiver_count(), 2);
    /// ```
    pub fn receiver_count(&self) -> usize {
        // Relaxed: a count; callers that act on it re-check as `closed` does.
        self.shared.ref_count_rx.load(Ordering::Relaxed)
    }

    /// How many senders exist now.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, _rx) = watch::channel("hello");
    /// assert_eq!(tx.sender_count(), 1);
    /// let tx2 = tx.clone();
    /// assert_eq!(tx2.sender_count(), 2);
    /// ```
    pub fn sender_count(&self) -> usize {
        // Relaxed: as `receiver_count`.
        self.shared.ref_count_tx.load(Ordering::Relaxed)
    }

    /// Whether the two senders belong to the same channel.
    ///
    /// ```
    /// use nuclei::sync::watch;
    ///
    /// let (tx, _rx) = watch::channel(true);
    /// let tx2 = tx.clone();
    /// assert!(tx.same_channel(&tx2));
    /// let (tx3, _rx3) = watch::channel(true);
    /// assert!(!tx3.same_channel(&tx2));
    /// ```
    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl<T: Send + Sync + 'static> Clone for Sender<T> {
    fn clone(&self) -> Self {
        // Relaxed: a plain counter, and the clone proves a sender exists.
        self.shared.ref_count_tx.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T: Send + Sync + 'static + Default> Default for Sender<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: Send + Sync + 'static> Drop for Sender<T> {
    fn drop(&mut self) {
        // AcqRel: the thread that drops the last sender sees every earlier
        // sender's sends before it closes the channel.
        if self.shared.ref_count_tx.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Release: a receiver that sees CLOSED sees the last version.
            self.shared.state.fetch_or(CLOSED, Ordering::Release);
            self.shared.notify_rx.notify_waiters();
        }
    }
}

impl<T: Send + Sync + 'static + fmt::Debug> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("shared", &*self.shared)
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn the_closed_bit_leaves_the_version_alone() {
        let (tx, rx) = channel(0u8);
        tx.send(1).unwrap();
        let before = version_of(tx.shared.state.load(Ordering::Acquire));
        drop(tx);
        assert!(rx.is_closed());
        assert_eq!(version_of(rx.shared.state.load(Ordering::Acquire)), before);
    }

    #[test]
    fn the_version_wraps_without_touching_the_closed_bit() {
        let (tx, rx) = channel(0u8);
        tx.shared.state.store(u64::MAX - 1, Ordering::SeqCst);
        tx.send(1).unwrap();
        let state = tx.shared.state.load(Ordering::SeqCst);
        assert_eq!(state & CLOSED, 0, "a wrapped version is not a close");
        assert_eq!(version_of(state), 0);
        assert!(!rx.is_closed());
    }
}
