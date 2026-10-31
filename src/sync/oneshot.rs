//! A channel that carries one value from one sender to one receiver.
//!
//! [`channel`] returns a [`Sender`] and a [`Receiver`]. The sender sends at
//! most one value and never waits; the receiver is a [`Future`] that
//! completes with the value, or with a [`error::RecvError`] when
//! the sender is dropped without sending. The API is the established async
//! one-shot channel API; nothing here needs a runtime, and every wait is a
//! plain [`Waker`](std::task::Waker) handshake.
//!
//! ```
//! use nuclei::sync::oneshot;
//!
//! let (tx, rx) = oneshot::channel();
//! std::thread::spawn(move || {
//!     tx.send(3).expect("the receiver is still there");
//! });
//! assert_eq!(rx.blocking_recv(), Ok(3));
//! ```
//!
//! If the sender is dropped without sending, the receiver fails:
//!
//! ```
//! use nuclei::sync::oneshot;
//!
//! let (tx, rx) = oneshot::channel::<u32>();
//! drop(tx);
//! assert!(rx.blocking_recv().is_err());
//! ```
//!
//! A value that was sent but never received stays in the channel until the
//! receiver is dropped, which drops it on the spot.
//!
//! # Progress
//!
//! Every operation of the channel is wait-free: it runs a bounded number of
//! atomic read-modify-writes and loads of its own, and none of them loops
//! because another thread got in first or waits for another thread. That
//! covers [`Sender::send`], [`Sender::is_closed`], [`Sender::poll_closed`],
//! every poll of the [`Receiver`], [`Receiver::try_recv`],
//! [`Receiver::close`], [`Receiver::is_empty`], [`Receiver::is_terminated`]
//! and the drop of either half. Three things are not counted. A [`Waker`](std::task::Waker)
//! the channel calls is the executor's, and so is what it does; the
//! destructor of the value runs in the call that drops it; and
//! [`Receiver::blocking_recv`] parks the calling thread until the value
//! arrives, which is what it is for.
//!
//! # How the halves agree on the value
//!
//! The value lives in one cell, `UnsafeCell<MaybeUninit<T>>`, and the state
//! word has three bits: `VALUE_SENT`, `RX_CLOSED` and `TX_DROPPED`. Every
//! bit is set once, with one `fetch_or`, so the word has one modification
//! order that both halves read their outcome from. The cell has exactly one
//! owner at a time:
//!
//! 1. Before the sender's `fetch_or`, the sender owns the cell and the
//!    receiver never reads it: the receiver reads it only after it saw
//!    `VALUE_SENT` in a word it loaded with `Acquire`, or in the word its own
//!    `fetch_or` returned.
//! 2. `send` writes the value, then does one `fetch_or(VALUE_SENT |
//!    TX_DROPPED)` and looks at the word that came back.
//!    - `RX_CLOSED` was clear: the receiver's `fetch_or(RX_CLOSED)`, if it
//!      ever happens, comes later in the order, so the receiver's own result
//!      shows `VALUE_SENT`. The send succeeded and the cell is the
//!      receiver's.
//!    - `RX_CLOSED` was set: the receiver's `fetch_or` came first and its
//!      result showed no `VALUE_SENT`, so the receiver wrote down that its
//!      channel closed empty (`RxClose::Empty`) and never reads the cell,
//!      whatever bits show up later. The sender reads its value back out and
//!      returns it.
//! 3. The receiver's side reads the cell exactly once, in the first of: a
//!    poll or `try_recv` that sees `VALUE_SENT` (it takes the value out and
//!    forgets the channel), or `Receiver::drop` (it drops the value if
//!    `VALUE_SENT` was set when it closed). A receiver that closed empty
//!    skips all of these.
//!
//! So a value is read by the sender (a failed send), by the receiver's take
//! (a received value), or by the receiver's drop (an unreceived value), never
//! by two of them, and never leaked while both halves honour the protocol:
//! the cell needs no drop glue of its own.
//!
//! # Wakeups
//!
//! The receiver's task and the sender's task each have an
//! `AtomicWaker`. A poll registers its
//! waker and then looks at the state word again, and a state change is
//! followed by a wake of the other side's waker, so a change is either seen
//! by the re-check or woken: see the waker's module for the handshake.

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::atomic_waker::AtomicWaker;
use super::internal::{AtomicUsize, BlockArc, Ordering, UnsafeCell};

/// The sender wrote the value and published it, in the same step that
/// raised `TX_DROPPED`.
const VALUE_SENT: usize = 0b001;
/// The receiver closed or was dropped: no later send can succeed.
const RX_CLOSED: usize = 0b010;
/// The sender is gone: it sent, or it was dropped without sending.
const TX_DROPPED: usize = 0b100;

/// Error types of the one-shot channel.
pub mod error {
    use core::fmt;

    /// Error returned by the `Future` implementation of
    /// [`Receiver`](super::Receiver).
    ///
    /// The receiver gets it when the sender is dropped without sending, or
    /// when the receiver was closed with nothing sent.
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub struct RecvError(pub(crate) ());

    /// Error returned by [`Receiver::try_recv`](super::Receiver::try_recv).
    #[derive(Debug, Eq, PartialEq, Clone)]
    pub enum TryRecvError {
        /// The send half of the channel has not yet sent a value.
        Empty,

        /// The send half of the channel was dropped without sending a value,
        /// or the value has already been received.
        Closed,
    }

    impl fmt::Display for RecvError {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(fmt, "channel closed")
        }
    }

    impl std::error::Error for RecvError {}

    impl fmt::Display for TryRecvError {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                TryRecvError::Empty => write!(fmt, "channel empty"),
                TryRecvError::Closed => write!(fmt, "channel closed"),
            }
        }
    }

    impl std::error::Error for TryRecvError {}
}

use self::error::{RecvError, TryRecvError};

/// What the receiver wrote down about its own close; see the module docs for
/// why it must (the state word alone cannot say who owns the cell once the
/// receiver closed).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RxClose {
    /// The receiver has not closed.
    Open,
    /// It closed after the value was sent: the cell is the receiver's.
    WithValue,
    /// It closed before any value was sent: the cell is never the receiver's.
    Empty,
}

struct Inner<T> {
    state: AtomicUsize,
    /// Written once by the sender before it raises `VALUE_SENT`; read once by
    /// the receiver after it saw the bit. Ownership: see the module docs.
    value: UnsafeCell<MaybeUninit<T>>,
    /// The receiver's task, woken when the state changes in its favour.
    rx_waker: AtomicWaker,
    /// The sender's task, woken when the receiver closes.
    tx_waker: AtomicWaker,
}

// SAFETY: the cell is accessed by one side at a time (module docs), and a `T`
// crosses from the sender's thread to the receiver's, so `T: Send` is the
// whole requirement; no `&T` is ever shared.
unsafe impl<T: Send> Send for Inner<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T> Inner<T> {
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            value: UnsafeCell::new(MaybeUninit::uninit()),
            rx_waker: AtomicWaker::new(),
            tx_waker: AtomicWaker::new(),
        }
    }

    /// Moves the value out of the cell.
    ///
    /// # Safety
    ///
    /// The caller owns the cell (module docs) and the value is initialised
    /// and not yet read.
    unsafe fn read_value(&self) -> T {
        // SAFETY: the caller's contract.
        self.value
            .with_mut(|slot| unsafe { (*slot).assume_init_read() })
    }

    /// The receiver's outcome against `state`, a word it loaded with
    /// `Acquire`: `Some(Some(value))`, `Some(None)` when nothing will ever
    /// come, or `None` while the channel is open and empty.
    ///
    /// # Safety
    ///
    /// Called by the receiver, once per value: the caller drops its handle
    /// when this returns `Some`.
    unsafe fn resolve(&self, close: RxClose, state: usize) -> Option<Option<T>> {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.resolve_if(close, state, || true) }.ok()?
    }

    /// [`resolve`](Self::resolve) that asks `admit` once the receive would
    /// complete and before it reads the value: `Err(())` when `admit` says
    /// no, the cell untouched.
    ///
    /// # Safety
    ///
    /// As for [`resolve`](Self::resolve), for an answer that is not `Err`.
    #[inline]
    unsafe fn resolve_if(
        &self,
        close: RxClose,
        state: usize,
        admit: impl FnOnce() -> bool,
    ) -> Result<Option<Option<T>>, ()> {
        let completes = close == RxClose::Empty || state & (VALUE_SENT | TX_DROPPED) != 0;
        if completes && !admit() {
            return Err(());
        }
        Ok(if close == RxClose::Empty {
            Some(None)
        } else if state & VALUE_SENT != 0 {
            // SAFETY: the sender raised VALUE_SENT after writing the value, and
            // the receiver did not close empty, so the cell is the receiver's
            // (module docs, rule 2). The caller retires the handle on `Some`.
            Some(Some(unsafe { self.read_value() }))
        } else if state & TX_DROPPED != 0 {
            Some(None)
        } else {
            None
        })
    }
}

/// Creates a one-shot channel for sending a single value between tasks.
///
/// The [`Sender`] sends the value; the [`Receiver`] awaits it.
///
/// ```
/// use nuclei::sync::oneshot;
///
/// let (tx, mut rx) = oneshot::channel();
/// assert!(rx.try_recv().is_err());
/// tx.send("hello").expect("the receiver is still there");
/// assert_eq!(rx.try_recv(), Ok("hello"));
/// ```
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = BlockArc::new(Inner::new());
    let tx = Sender {
        inner: Some(BlockArc::clone(&inner)),
    };
    let rx = Receiver {
        inner: Some(inner),
        close: RxClose::Open,
    };
    (tx, rx)
}

/// Sends a value to the associated [`Receiver`].
///
/// A [`Sender`] and a [`Receiver`] are created together by [`channel`].
/// Every operation is wait-free; see the [module docs](self).
///
/// ```
/// use nuclei::sync::oneshot;
///
/// let (tx, rx) = oneshot::channel();
/// tx.send(3).expect("the receiver is still there");
/// assert_eq!(rx.blocking_recv(), Ok(3));
/// ```
///
/// To use a `Sender` from a destructor, put it in an [`Option`] and call
/// [`Option::take`].
pub struct Sender<T> {
    /// `None` only after `send` took it.
    inner: Option<BlockArc<Inner<T>>>,
}

impl<T> Sender<T> {
    /// Attempts to send a value on this channel, returning it back if it
    /// could not be sent.
    ///
    /// This method consumes `self`, since only one value may ever be sent on
    /// a `oneshot` channel. It never waits, so it can be used in both
    /// synchronous and asynchronous code.
    ///
    /// A successful send means the other end had not hung up when the value
    /// was published; an unsuccessful one means the [`Receiver`] was already
    /// closed or dropped, and the value comes back in the `Err`. `Ok` does
    /// *not* promise the value will be received: the receiver can hang up
    /// right after this returns.
    ///
    /// Wait-free: one write and one atomic read-modify-write, plus the wake
    /// of the receiver's task.
    ///
    /// ```
    /// use nuclei::sync::oneshot;
    ///
    /// let (tx, rx) = oneshot::channel();
    /// drop(rx);
    /// assert_eq!(tx.send(3), Err(3));
    /// ```
    pub fn send(mut self, value: T) -> Result<(), T> {
        let Some(inner) = self.inner.take() else {
            return Err(value);
        };
        // SAFETY: VALUE_SENT is raised only below, and this consumes the only
        // `Sender`, so the receiver cannot be reading the cell (module docs,
        // rule 1).
        inner.value.with_mut(|slot| unsafe {
            (*slot).write(value);
        });
        // AcqRel: the release publishes the write above to the receiver's
        // acquire loads; the acquire orders this word after the receiver's
        // close when it won the race.
        let prev = inner
            .state
            .fetch_or(VALUE_SENT | TX_DROPPED, Ordering::AcqRel);
        if prev & RX_CLOSED != 0 {
            // SAFETY: the receiver closed first and wrote down that no value
            // was there (module docs, rule 2), so the cell is still ours and
            // holds the value written above.
            return Err(unsafe { inner.read_value() });
        }
        inner.rx_waker.wake();
        Ok(())
    }

    /// Waits for the associated [`Receiver`] to close.
    ///
    /// A receiver closes by [`Receiver::close`] or by being dropped. This is
    /// useful with a select to abort a computation whose result nobody wants
    /// any more.
    ///
    /// ```
    /// use nuclei::sync::oneshot;
    /// use std::future::Future;
    /// use std::pin::pin;
    /// use std::task::{Context, Poll, Waker};
    ///
    /// let (mut tx, rx) = oneshot::channel::<()>();
    /// let mut cx = Context::from_waker(Waker::noop());
    /// {
    ///     let mut closed = pin!(tx.closed());
    ///     assert_eq!(closed.as_mut().poll(&mut cx), Poll::Pending);
    ///     drop(rx);
    ///     assert_eq!(closed.as_mut().poll(&mut cx), Poll::Ready(()));
    /// }
    /// ```
    pub async fn closed(&mut self) {
        std::future::poll_fn(|cx| self.poll_closed(cx)).await;
    }

    /// Whether the associated [`Receiver`] has closed or been dropped.
    ///
    /// When this is `true`, a call to [`send`](Self::send) always fails.
    ///
    /// ```
    /// use nuclei::sync::oneshot;
    ///
    /// let (tx, rx) = oneshot::channel::<u8>();
    /// assert!(!tx.is_closed());
    /// drop(rx);
    /// assert!(tx.is_closed());
    /// ```
    pub fn is_closed(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(|inner| inner.state.load(Ordering::Acquire) & RX_CLOSED != 0)
    }

    /// Checks whether the channel is closed and, if it is not, arranges for
    /// the waker of `cx` to be woken when it closes.
    ///
    /// Only the waker of the most recent call is kept.
    ///
    /// Returns [`Poll::Pending`] while the channel is open and
    /// [`Poll::Ready`] once the receiver closed. Wait-free.
    pub fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let Some(inner) = self.inner.as_ref() else {
            return Poll::Ready(());
        };
        if inner.state.load(Ordering::Acquire) & RX_CLOSED != 0 {
            return Poll::Ready(());
        }
        inner.tx_waker.register(cx.waker());
        // The re-check after the registration is what makes a close that
        // raced the registration visible; see the waker's handshake.
        if inner.state.load(Ordering::Acquire) & RX_CLOSED != 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        // AcqRel: the release hands the receiver this word; the acquire is for
        // the close it may have raised first.
        let prev = inner.state.fetch_or(TX_DROPPED, Ordering::AcqRel);
        // A receiver that closed polls to an answer without waiting.
        if prev & RX_CLOSED == 0 {
            inner.rx_waker.wake();
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// Receives a value from the associated [`Sender`].
///
/// A [`Sender`] and a [`Receiver`] are created together by [`channel`]. The
/// receiver is itself a [`Future`]: await it to get a
/// `Result<T, `[`error::RecvError`]`>`. There is no `recv`
/// method.
///
/// Polling a receiver never fails spuriously: a value sent before a poll
/// starts is returned by that poll.
///
/// # Cancel safety
///
/// Awaiting a `&mut Receiver<T>` is cancel safe. If it is a branch of a
/// select and another branch completes first, no value was received on this
/// channel.
///
/// # Panics
///
/// Polling a receiver again after it completed (it returned `Ready`, or
/// [`try_recv`](Self::try_recv) returned a value or
/// [`TryRecvError::Closed`]) panics with `called after complete`. Check
/// [`is_terminated`](Self::is_terminated) first.
///
/// ```
/// use nuclei::sync::oneshot;
/// use std::future::Future;
/// use std::pin::Pin;
/// use std::task::{Context, Poll, Waker};
///
/// let (tx, mut rx) = oneshot::channel();
/// let mut cx = Context::from_waker(Waker::noop());
/// assert_eq!(Pin::new(&mut rx).poll(&mut cx), Poll::Pending);
/// tx.send(7).expect("the receiver is still there");
/// assert_eq!(Pin::new(&mut rx).poll(&mut cx), Poll::Ready(Ok(7)));
/// assert!(rx.is_terminated());
/// ```
pub struct Receiver<T> {
    /// `None` once the receiver completed: it returned its value or an error.
    inner: Option<BlockArc<Inner<T>>>,
    close: RxClose,
}

impl<T> Receiver<T> {
    /// Prevents the associated [`Sender`] from sending a value.
    ///
    /// Any `send` that happens after `close` returns fails. A value sent
    /// *before* `close` can still be taken with [`try_recv`](Self::try_recv)
    /// (or by polling). This makes a graceful shutdown possible: close, then
    /// drain.
    ///
    /// `close` does nothing if the value was already received or the
    /// channel is already closed. Wait-free.
    ///
    /// ```
    /// use nuclei::sync::oneshot;
    ///
    /// let (tx, mut rx) = oneshot::channel();
    /// tx.send("sent before").expect("the receiver is still there");
    /// rx.close();
    /// assert_eq!(rx.try_recv(), Ok("sent before"));
    ///
    /// let (tx, mut rx) = oneshot::channel();
    /// rx.close();
    /// assert!(tx.is_closed());
    /// assert_eq!(tx.send("too late"), Err("too late"));
    /// ```
    pub fn close(&mut self) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        if self.close != RxClose::Open {
            return;
        }
        // AcqRel: the acquire pairs with the sender's release, so a value
        // that this word shows is visible; the release is for the sender.
        let prev = inner.state.fetch_or(RX_CLOSED, Ordering::AcqRel);
        self.close = if prev & VALUE_SENT != 0 {
            RxClose::WithValue
        } else {
            RxClose::Empty
        };
        // A sender that is gone has nobody to wake.
        if prev & TX_DROPPED == 0 {
            inner.tx_waker.wake();
        }
    }

    /// Whether this receiver already completed: it returned its value or an
    /// error, so it must not be polled again.
    ///
    /// ```
    /// use nuclei::sync::oneshot;
    ///
    /// let (tx, mut rx) = oneshot::channel();
    /// assert!(!rx.is_terminated());
    /// tx.send(0).expect("the receiver is still there");
    /// assert!(!rx.is_terminated(), "a value that was sent but not read");
    /// assert_eq!(rx.try_recv(), Ok(0));
    /// assert!(rx.is_terminated());
    /// ```
    pub fn is_terminated(&self) -> bool {
        self.inner.is_none()
    }

    /// Whether the channel holds no value.
    ///
    /// `true` before a send, after the sender was dropped, and after the
    /// receiver completed. It is not a promise that polling is safe: use
    /// [`is_terminated`](Self::is_terminated) for that.
    ///
    /// ```
    /// use nuclei::sync::oneshot;
    ///
    /// let (tx, rx) = oneshot::channel();
    /// assert!(rx.is_empty());
    /// tx.send(0).expect("the receiver is still there");
    /// assert!(!rx.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        let Some(inner) = self.inner.as_ref() else {
            return true;
        };
        self.close == RxClose::Empty || inner.state.load(Ordering::Acquire) & VALUE_SENT == 0
    }

    /// Attempts to receive a value without waiting.
    ///
    /// The current task is *not* registered for a later wake. Unlike a
    /// poll, it cannot fail spuriously: any send or close that happened
    /// before the call is reported. Wait-free.
    ///
    /// - `Ok(value)` when a value is waiting.
    /// - `Err(TryRecvError::Empty)` when none was sent yet.
    /// - `Err(TryRecvError::Closed)` when the sender was dropped without
    ///   sending, the channel was closed empty, or the value was received
    ///   already.
    ///
    /// ```
    /// use nuclei::sync::oneshot::{self, error::TryRecvError};
    ///
    /// let (tx, mut rx) = oneshot::channel();
    /// assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    /// tx.send("hello").expect("the receiver is still there");
    /// assert_eq!(rx.try_recv(), Ok("hello"));
    /// assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    /// ```
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let Some(inner) = self.inner.as_ref() else {
            return Err(TryRecvError::Closed);
        };
        let state = inner.state.load(Ordering::Acquire);
        // SAFETY: the handle is retired just below whenever this returns `Some`.
        match unsafe { inner.resolve(self.close, state) } {
            Some(done) => {
                self.inner = None;
                done.ok_or(TryRecvError::Closed)
            }
            None => Err(TryRecvError::Empty),
        }
    }

    /// Receives the value on this thread, parking the thread until it
    /// arrives.
    ///
    /// For use outside asynchronous code. On a nuclei shard (inside one of its tasks) it does not park the thread: it runs a nested `block_on`, so the shard keeps running its other tasks meanwhile. Inside another runtime's task
    /// it blocks that task's executor thread, so do not. It is not wait-free:
    /// waiting for the sender is what it does.
    ///
    /// # Panics
    ///
    /// Panics if the receiver already completed; see [`Receiver`]. On
    /// `wasm32`, where no other thread can send, it also panics when the
    /// value is not there already: the call could never end.
    ///
    /// ```
    /// use nuclei::sync::oneshot;
    ///
    /// let (tx, rx) = oneshot::channel::<u8>();
    /// let sender = std::thread::spawn(move || tx.send(10));
    /// assert_eq!(rx.blocking_recv(), Ok(10));
    /// sender.join().expect("sender thread").expect("the receiver was waiting");
    /// ```
    pub fn blocking_recv(self) -> Result<T, RecvError> {
        super::internal::blocking(self)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        let owns_value = match self.close {
            RxClose::WithValue => true,
            RxClose::Empty => false,
            RxClose::Open => {
                // AcqRel: as in `close`.
                let prev = inner.state.fetch_or(RX_CLOSED, Ordering::AcqRel);
                if prev & TX_DROPPED == 0 {
                    inner.tx_waker.wake();
                }
                prev & VALUE_SENT != 0
            }
        };
        if owns_value {
            // SAFETY: VALUE_SENT was set before this receiver closed, so the
            // cell is the receiver's (module docs, rule 2) and nothing read it:
            // a receiver that read it has no `inner`.
            drop(unsafe { inner.read_value() });
        }
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    #[expect(
        clippy::panic,
        reason = "polled after it completed, which the `Future` contract rules out"
    )]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_ref() else {
            panic!("called after complete");
        };
        let mut state = inner.state.load(Ordering::Acquire);
        let mut registered = false;
        // A receive that completes spends one operation of the poll's budget;
        // with the budget spent it takes nothing and yields (see `coop`).
        let admit = || crate::coop::spend(cx.waker());
        loop {
            // SAFETY: the handle is retired just below whenever this returns `Some`.
            match unsafe { inner.resolve_if(this.close, state, admit) } {
                Ok(Some(done)) => {
                    this.inner = None;
                    return Poll::Ready(done.ok_or(RecvError(())));
                }
                Err(()) => return Poll::Pending,
                Ok(None) => {}
            }
            if registered {
                return Poll::Pending;
            }
            inner.rx_waker.register(cx.waker());
            registered = true;
            // The re-check after the registration is what makes a send that
            // raced the registration visible; see the waker's handshake.
            state = inner.state.load(Ordering::Acquire);
        }
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("is_terminated", &self.is_terminated())
            .field("is_empty", &self.is_empty())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::{Polled, block_on};
    use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};

    /// Counts how many values were dropped.
    struct DropCount(std::sync::Arc<StdAtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, StdOrdering::SeqCst);
        }
    }

    fn counted() -> (std::sync::Arc<StdAtomicUsize>, DropCount) {
        let drops = std::sync::Arc::new(StdAtomicUsize::new(0));
        (std::sync::Arc::clone(&drops), DropCount(drops))
    }

    #[test]
    fn a_pending_receiver_is_woken_by_the_send() {
        let (tx, rx) = channel();
        let mut rx = Polled::new(rx);
        rx.pending();
        assert!(tx.send(5).is_ok());
        assert_eq!(rx.wakes(), 1);
        assert_eq!(rx.ready(), Ok(5));
    }

    #[test]
    fn a_pending_sender_is_woken_by_the_close() {
        let (mut tx, mut rx) = channel::<u8>();
        let mut closed = Polled::new(std::future::poll_fn(|cx| tx.poll_closed(cx)));
        closed.pending();
        rx.close();
        assert_eq!(closed.wakes(), 1);
        closed.ready();
        drop(closed);
        assert_eq!(tx.send(1), Err(1));
    }

    #[test]
    fn an_unreceived_value_is_dropped_by_the_receiver() {
        let (drops, value) = counted();
        let (tx, rx) = channel();
        assert!(tx.send(value).is_ok());
        assert_eq!(drops.load(StdOrdering::SeqCst), 0, "the channel holds it");
        drop(rx);
        assert_eq!(drops.load(StdOrdering::SeqCst), 1);
    }

    #[test]
    fn a_closed_receiver_still_owns_a_value_sent_before_the_close() {
        let (drops, value) = counted();
        let (tx, mut rx) = channel();
        assert!(tx.send(value).is_ok());
        rx.close();
        assert_eq!(drops.load(StdOrdering::SeqCst), 0);
        drop(rx);
        assert_eq!(drops.load(StdOrdering::SeqCst), 1);
    }

    #[test]
    fn a_failed_send_returns_the_value_and_drops_it_once() {
        let (drops, value) = counted();
        let (tx, mut rx) = channel();
        rx.close();
        let back = tx.send(value).expect_err("the receiver closed");
        assert_eq!(drops.load(StdOrdering::SeqCst), 0, "returned, not dropped");
        drop(back);
        drop(rx);
        assert_eq!(drops.load(StdOrdering::SeqCst), 1);
    }

    #[test]
    fn a_received_value_is_not_dropped_by_the_channel() {
        let (drops, value) = counted();
        let (tx, mut rx) = channel();
        assert!(tx.send(value).is_ok());
        let value = rx.try_recv().expect("sent");
        drop(rx);
        assert_eq!(drops.load(StdOrdering::SeqCst), 0);
        drop(value);
        assert_eq!(drops.load(StdOrdering::SeqCst), 1);
    }

    #[test]
    fn a_value_sent_across_threads_arrives_and_drops_once() {
        for _ in 0..if cfg!(miri) { 5 } else { 500 } {
            let (drops, value) = counted();
            let (tx, rx) = channel();
            let sender = std::thread::spawn(move || tx.send(value).is_ok());
            let got = block_on(rx);
            assert!(sender.join().expect("sender"));
            drop(got);
            assert_eq!(drops.load(StdOrdering::SeqCst), 1);
        }
    }

    #[test]
    #[should_panic(expected = "called after complete")]
    fn polling_a_completed_receiver_panics() {
        let (tx, rx) = channel();
        assert!(tx.send(1).is_ok());
        let mut rx = Polled::new(rx);
        assert_eq!(rx.ready(), Ok(1));
        let _ = rx.poll();
    }
}
