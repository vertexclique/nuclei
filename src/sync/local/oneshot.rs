//! A channel that carries one value from one sender to one receiver, both
//! on one shard.
//!
//! The names, signatures and errors follow [`sync::oneshot`](crate::sync::oneshot),
//! so code switches by changing the path. The state is one `Rc` with a
//! `Cell` of three bits (value sent, receiver closed, sender dropped), the
//! value in a cell and the two wakers in cells: no atomic and no fence. The
//! sender sends at most one value and never waits; the receiver is a
//! [`Future`] that completes with the value, or with a
//! [`RecvError`] when the sender is dropped without sending, and, like the
//! atomic one, panics when polled again after it completed. A value that was
//! sent and never received stays in the channel until the receiver is
//! dropped, which drops it on the spot (outside any borrow).
//!
//! A receive that completes spends one operation of the cooperative budget
//! (see [`task::coop`](crate::task::coop)).

#![allow(unsafe_code)]

use core::cell::{Cell, UnsafeCell};
use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::rc::Rc;

pub use crate::sync::oneshot::error;
use crate::sync::oneshot::error::{RecvError, TryRecvError};

const VALUE_SENT: u8 = 0b001;
const RX_CLOSED: u8 = 0b010;
const TX_DROPPED: u8 = 0b100;

struct Inner<T> {
    state: Cell<u8>,
    value: UnsafeCell<Option<T>>,
    rx_waker: Cell<Option<Waker>>,
    tx_waker: Cell<Option<Waker>>,
}

impl<T> Inner<T> {
    fn has(&self, bit: u8) -> bool {
        self.state.get() & bit != 0
    }

    fn set(&self, bit: u8) {
        self.state.set(self.state.get() | bit);
    }

    fn put(&self, value: T) {
        // SAFETY: `!Sync` (an `Rc`), and this step runs no user code and
        // calls no other step; the cell is empty until the one send.
        unsafe { *self.value.get() = Some(value) };
    }

    fn take(&self) -> Option<T> {
        // SAFETY: as in `put`.
        unsafe { (*self.value.get()).take() }
    }
}

/// Replaces the waker in `slot` with `waker` unless it already wakes the
/// same task; the old one is dropped after the cell is set.
fn register(slot: &Cell<Option<Waker>>, waker: &Waker) {
    match slot.take() {
        Some(old) if old.will_wake(waker) => slot.set(Some(old)),
        old => {
            slot.set(Some(waker.clone()));
            drop(old);
        }
    }
}

fn wake(slot: &Cell<Option<Waker>>) {
    if let Some(waker) = slot.take() {
        waker.wake();
    }
}

/// Creates a one-shot channel and answers its two halves.
///
/// The halves belong to the tasks of one shard: they are `!Send`.
///
/// ```
/// use nuclei::sync::local::oneshot;
///
/// nuclei::runtime::block_on(async {
///     let (tx, rx) = oneshot::channel();
///     tx.send(3).unwrap();
///     assert_eq!(rx.await, Ok(3));
/// });
/// ```
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Rc::new(Inner {
        state: Cell::new(0),
        value: UnsafeCell::new(None),
        rx_waker: Cell::new(None),
        tx_waker: Cell::new(None),
    });
    (
        Sender {
            inner: Some(Rc::clone(&inner)),
        },
        Receiver { inner: Some(inner) },
    )
}

/// Sends one value to the [`Receiver`].
///
/// Dropping it without sending ends the receiver's wait with a
/// [`RecvError`].
pub struct Sender<T> {
    inner: Option<Rc<Inner<T>>>,
}

impl<T> Sender<T> {
    /// Sends the value and wakes the receiver. Never waits.
    ///
    /// # Errors
    ///
    /// Gives the value back when the receiver is closed or gone.
    pub fn send(mut self, value: T) -> Result<(), T> {
        let Some(inner) = self.inner.take() else {
            return Err(value);
        };
        if inner.has(RX_CLOSED) {
            inner.set(TX_DROPPED);
            return Err(value);
        }
        inner.put(value);
        inner.set(VALUE_SENT | TX_DROPPED);
        wake(&inner.rx_waker);
        Ok(())
    }

    /// Completes when the receiver has been closed or dropped. Cancel safe.
    pub async fn closed(&mut self) {
        core::future::poll_fn(|cx| self.poll_closed(cx)).await;
    }

    /// Whether the receiver has been closed or dropped.
    pub fn is_closed(&self) -> bool {
        self.inner.as_ref().is_none_or(|inner| inner.has(RX_CLOSED))
    }

    /// Polls whether the receiver has been closed or dropped; the waker of
    /// `cx` is woken when it is.
    pub fn poll_closed(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let Some(inner) = self.inner.as_ref() else {
            return Poll::Ready(());
        };
        if inner.has(RX_CLOSED) {
            return Poll::Ready(());
        }
        register(&inner.tx_waker, cx.waker());
        Poll::Pending
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        inner.set(TX_DROPPED);
        wake(&inner.rx_waker);
        drop(inner.tx_waker.take());
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("is_closed", &self.is_closed())
            .finish()
    }
}

/// Receives the value of the [`Sender`]; a future.
///
/// Cancel safe: dropping the future before it completes loses nothing the
/// channel still holds (a value sent later is dropped with the receiver).
pub struct Receiver<T> {
    inner: Option<Rc<Inner<T>>>,
}

impl<T> Receiver<T> {
    /// Closes the receiving half: a later send fails and gets its value
    /// back. A value sent before the close stays receivable.
    pub fn close(&mut self) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        inner.set(RX_CLOSED);
        wake(&inner.tx_waker);
    }

    /// Whether the receiver completed or failed already.
    pub fn is_terminated(&self) -> bool {
        self.inner.is_none()
    }

    /// Whether no value is waiting for the receiver.
    pub fn is_empty(&self) -> bool {
        self.inner
            .as_ref()
            .is_none_or(|inner| !inner.has(VALUE_SENT))
    }

    /// Receives the value without waiting.
    ///
    /// # Errors
    ///
    /// [`TryRecvError::Empty`] if no value was sent yet and the sender is
    /// still there; [`TryRecvError::Closed`] if the sender dropped without
    /// sending, the receiver was closed empty, or the value was received.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let Some(inner) = self.inner.as_ref() else {
            return Err(TryRecvError::Closed);
        };
        if let Some(value) = inner.take() {
            self.inner = None;
            return Ok(value);
        }
        if inner.has(RX_CLOSED | TX_DROPPED) {
            self.inner = None;
            return Err(TryRecvError::Closed);
        }
        Err(TryRecvError::Empty)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        inner.set(RX_CLOSED);
        wake(&inner.tx_waker);
        drop(inner.rx_waker.take());
        drop(inner.take());
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
        if !inner.has(VALUE_SENT | RX_CLOSED | TX_DROPPED) {
            register(&inner.rx_waker, cx.waker());
            return Poll::Pending;
        }
        if crate::coop::exhausted(cx.waker()) {
            return Poll::Pending;
        }
        let done = inner.take().ok_or(RecvError(()));
        this.inner = None;
        crate::coop::spent_one();
        Poll::Ready(done)
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
