//! The unbounded single-shard channel: a send never waits.

use core::fmt;
use std::rc::Rc;

use super::chan::{Chan, Limit};
use crate::sync::mpsc::error::SendError;

pub(super) struct Unbounded;

impl Limit for Unbounded {
    fn release(&self, _n: usize) {}

    fn is_idle(&self) -> bool {
        true
    }

    fn close(&self) {}
}

/// Creates an unbounded channel and answers its two halves.
///
/// The halves belong to the tasks of one shard: they are `!Send`.
///
/// ```
/// use nuclei::sync::local::mpsc;
///
/// let (tx, mut rx) = mpsc::unbounded_channel();
/// tx.send(1).unwrap();
/// tx.send(2).unwrap();
/// assert_eq!(rx.try_recv(), Ok(1));
/// assert_eq!(rx.try_recv(), Ok(2));
/// ```
pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    let chan = Rc::new(Chan::new(Unbounded));
    let tx = UnboundedSender {
        chan: Rc::clone(&chan),
    };
    (tx, UnboundedReceiver { chan })
}

/// Sends values to the associated [`UnboundedReceiver`].
///
/// Created by [`unbounded_channel`]. Cloning it counts one more sender.
pub struct UnboundedSender<T> {
    chan: Rc<Chan<T, Unbounded>>,
}

impl<T> UnboundedSender<T> {
    /// Sends a value; it never waits.
    ///
    /// # Errors
    ///
    /// A [`SendError`] holding the value when the receiver is closed or gone.
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        if self.chan.is_closed() {
            return Err(SendError(value));
        }
        self.chan.push(value);
        Ok(())
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
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        self.chan.add_sender();
        Self {
            chan: Rc::clone(&self.chan),
        }
    }
}

impl<T> Drop for UnboundedSender<T> {
    fn drop(&mut self) {
        self.chan.drop_sender();
    }
}

impl<T> fmt::Debug for UnboundedSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnboundedSender")
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// Receives values from the associated [`UnboundedSender`]s.
///
/// Created by [`unbounded_channel`]. Dropping it closes the channel and drops
/// the values still queued.
pub struct UnboundedReceiver<T> {
    chan: Rc<Chan<T, Unbounded>>,
}

impl<T> UnboundedReceiver<T> {
    receiver_methods!();
}

impl<T> Drop for UnboundedReceiver<T> {
    fn drop(&mut self) {
        self.chan.drop_rx();
    }
}

impl<T> fmt::Debug for UnboundedReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnboundedReceiver")
            .field("len", &self.len())
            .field("closed", &self.is_closed())
            .finish()
    }
}
