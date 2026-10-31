//! The bounded single-shard channel: a queue plus a slot semaphore.
//!
//! A send acquires one slot (waiting in line when none is free), pushes its
//! value and wakes the receiver; a receive pops a value and gives its slot
//! back, which serves the first waiting send.

use core::fmt;
use core::future::Future;
use core::mem;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;

use super::chan::{Chan, Limit};
use crate::sync::TryAcquireError;
use crate::sync::local::semaphore::{Acquire, Semaphore};
use crate::sync::mpsc::error::{SendError, TrySendError};

pub(super) struct Bounded {
    slots: Semaphore,
    max: usize,
}

impl Limit for Bounded {
    fn release(&self, n: usize) {
        if n > 0 {
            self.slots.add_permits(n);
        }
    }

    fn is_idle(&self) -> bool {
        self.slots.available_permits() == self.max
    }

    fn close(&self) {
        self.slots.close();
    }
}

/// Creates a bounded channel that holds at most `buffer` messages, and
/// answers its two halves.
///
/// The halves belong to the tasks of one shard: they are `!Send`. A send on a
/// full channel waits in line for room.
///
/// # Panics
///
/// When `buffer` is zero, or more than [`Semaphore::MAX_PERMITS`].
///
/// ```
/// use nuclei::sync::local::mpsc;
///
/// let (tx, mut rx) = mpsc::channel(2);
/// tx.try_send(1).unwrap();
/// tx.try_send(2).unwrap();
/// assert!(tx.try_send(3).is_err());
/// assert_eq!(rx.try_recv(), Ok(1));
/// ```
#[track_caller]
pub fn channel<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    assert!(buffer > 0, "mpsc bounded channel requires buffer > 0");
    let chan = Rc::new(Chan::new(Bounded {
        slots: Semaphore::new(buffer),
        max: buffer,
    }));
    let tx = Sender {
        chan: Rc::clone(&chan),
    };
    (tx, Receiver { chan })
}

/// Sends values to the associated [`Receiver`].
///
/// Created by [`channel`]. Cloning it counts one more sender.
pub struct Sender<T> {
    chan: Rc<Chan<T, Bounded>>,
}

impl<T> Sender<T> {
    /// Sends a value, waiting in line for room if the channel is full.
    ///
    /// # Errors
    ///
    /// A [`SendError`] holding the value when the receiver is closed or gone.
    ///
    /// # Cancel safety
    ///
    /// A dropped future sent nothing and drops its value; it holds no slot.
    ///
    /// ```
    /// use nuclei::sync::local::mpsc;
    ///
    /// nuclei::runtime::block_on(async {
    ///     let (tx, mut rx) = mpsc::channel(1);
    ///     tx.send(1).await.unwrap();
    ///     assert_eq!(rx.recv().await, Some(1));
    /// });
    /// ```
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        match self.reserve().await {
            Ok(permit) => {
                permit.send(value);
                Ok(())
            }
            Err(_) => Err(SendError(value)),
        }
    }

    /// Sends a value at once, or says why it cannot be sent. It never takes
    /// a slot ahead of a send or reservation that is waiting.
    ///
    /// # Errors
    ///
    /// [`TrySendError::Full`] if no slot is free or a send is waiting,
    /// [`TrySendError::Closed`] if the receiver closed or was dropped. Both
    /// hold the value back.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        match self.try_reserve() {
            Ok(permit) => {
                permit.send(value);
                Ok(())
            }
            Err(TrySendError::Full(())) => Err(TrySendError::Full(value)),
            Err(TrySendError::Closed(())) => Err(TrySendError::Closed(value)),
        }
    }

    /// Waits in line for a slot and reserves it: the send through the
    /// [`Permit`] never waits and never fails.
    ///
    /// # Errors
    ///
    /// A [`SendError`] when the receiver is closed or gone.
    ///
    /// # Cancel safety
    ///
    /// A dropped future holds no slot.
    pub async fn reserve(&self) -> Result<Permit<'_, T>, SendError<()>> {
        Reserve {
            chan: &self.chan,
            acquire: self.chan.limit.slots.acquire(),
        }
        .await
    }

    /// Reserves a slot at once if one is free and nobody waits.
    ///
    /// # Errors
    ///
    /// [`TrySendError::Full`] if no slot is free or a send is waiting,
    /// [`TrySendError::Closed`] if the receiver closed or was dropped.
    pub fn try_reserve(&self) -> Result<Permit<'_, T>, TrySendError<()>> {
        match self.chan.limit.slots.try_acquire() {
            Ok(permit) => {
                permit.forget();
                Ok(Permit { chan: &self.chan })
            }
            Err(TryAcquireError::NoPermits) => Err(TrySendError::Full(())),
            Err(TryAcquireError::Closed) => Err(TrySendError::Closed(())),
        }
    }

    /// Completes when the receiver has been closed or dropped. Cancel safe;
    /// once the channel is closed every call completes at once.
    pub async fn closed(&self) {
        self.chan.closed().await;
    }

    /// Whether the receiver has been closed or dropped.
    pub fn is_closed(&self) -> bool {
        self.chan.is_closed()
    }

    /// Whether both senders send to the same channel.
    pub fn same_channel(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.chan, &other.chan)
    }

    /// The channel's current capacity: the slots that are free.
    pub fn capacity(&self) -> usize {
        self.chan.limit.slots.available_permits()
    }

    /// The channel's maximum capacity: the `buffer` it was created with.
    pub fn max_capacity(&self) -> usize {
        self.chan.limit.max
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.chan.add_sender();
        Self {
            chan: Rc::clone(&self.chan),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.chan.drop_sender();
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("capacity", &self.capacity())
            .field("max_capacity", &self.max_capacity())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// The wait of [`Sender::reserve`]. Its drop passes on a slot that was
/// granted and not yet taken, and tells a receiver that waits for the
/// channel to go idle.
struct Reserve<'a, T> {
    chan: &'a Chan<T, Bounded>,
    acquire: Acquire<'a>,
}

impl<'a, T> Future for Reserve<'a, T> {
    type Output = Result<Permit<'a, T>, SendError<()>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.acquire).poll(cx) {
            Poll::Ready(Ok(permit)) => {
                permit.forget();
                Poll::Ready(Ok(Permit { chan: this.chan }))
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(SendError(()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for Reserve<'_, T> {
    fn drop(&mut self) {
        self.acquire.cancel();
        self.chan.wake_if_finished();
    }
}

/// Room for one message, reserved before the message exists.
///
/// The send through it never waits and never fails, even when the receiver
/// was closed in the meantime. Dropping it without sending gives the slot
/// back.
///
/// ```
/// use nuclei::sync::local::mpsc;
///
/// let (tx, mut rx) = mpsc::channel(1);
/// let permit = tx.try_reserve().unwrap();
/// permit.send("reserved");
/// assert_eq!(rx.try_recv(), Ok("reserved"));
/// ```
#[must_use = "dropping a permit gives its slot back unused"]
pub struct Permit<'a, T> {
    chan: &'a Chan<T, Bounded>,
}

impl<T> Permit<'_, T> {
    /// Sends a value through the reserved slot, consuming the permit.
    pub fn send(self, value: T) {
        let chan = self.chan;
        // The slot goes to the value, not back to the semaphore.
        mem::forget(self);
        chan.push(value);
    }
}

impl<T> Drop for Permit<'_, T> {
    fn drop(&mut self) {
        self.chan.give_back(1);
    }
}

impl<T> fmt::Debug for Permit<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit").finish_non_exhaustive()
    }
}

/// Receives values from the associated [`Sender`]s.
///
/// Created by [`channel`]. Dropping it closes the channel and drops the
/// values still queued.
pub struct Receiver<T> {
    chan: Rc<Chan<T, Bounded>>,
}

impl<T> Receiver<T> {
    receiver_methods!();

    /// The channel's current capacity: the slots that are free.
    pub fn capacity(&self) -> usize {
        self.chan.limit.slots.available_permits()
    }

    /// The channel's maximum capacity: the `buffer` it was created with.
    pub fn max_capacity(&self) -> usize {
        self.chan.limit.max
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.chan.drop_rx();
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("len", &self.len())
            .field("closed", &self.is_closed())
            .finish()
    }
}
