//! Error types of the multi-producer, single-consumer channels.
//!
//! They carry the unsent value back to the caller, so a failed send never
//! loses it, and their `Display` text is the established one.

use core::fmt;
use std::error::Error;

/// Error returned by [`Sender::send`](super::Sender::send),
/// [`UnboundedSender::send`](super::UnboundedSender::send) and the other sends
/// that wait.
///
/// The channel is closed: the receiver was dropped or closed. The value that
/// could not be sent is returned in the error.
///
/// ```
/// use nuclei::sync::mpsc;
///
/// let (tx, rx) = mpsc::unbounded_channel();
/// drop(rx);
/// let err = tx.send("lost").unwrap_err();
/// assert_eq!(err.0, "lost");
/// assert_eq!(err.to_string(), "channel closed");
/// ```
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

/// Error returned by [`Sender::try_send`](super::Sender::try_send) and the
/// other sends and reservations that never wait.
///
/// ```
/// use nuclei::sync::mpsc::{self, error::TrySendError};
///
/// let (tx, rx) = mpsc::channel(1);
/// tx.try_send(1).unwrap();
/// assert!(matches!(tx.try_send(2), Err(TrySendError::Full(2))));
/// drop(rx);
/// assert!(matches!(tx.try_send(3), Err(TrySendError::Closed(3))));
/// ```
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum TrySendError<T> {
    /// The data could not be sent on the channel because the channel is
    /// currently full and sending would require blocking.
    Full(T),

    /// The receive half of the channel was explicitly closed or has been
    /// dropped.
    Closed(T),
}

impl<T> TrySendError<T> {
    /// Consume the `TrySendError`, returning the unsent value.
    pub fn into_inner(self) -> T {
        match self {
            TrySendError::Full(val) => val,
            TrySendError::Closed(val) => val,
        }
    }

    /// The same refusal, carrying `value` in place of this one's.
    pub(super) fn with_value<U>(self, value: U) -> TrySendError<U> {
        match self {
            TrySendError::Full(_) => TrySendError::Full(value),
            TrySendError::Closed(_) => TrySendError::Closed(value),
        }
    }
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TrySendError::Full(..) => "Full(..)".fmt(f),
            TrySendError::Closed(..) => "Closed(..)".fmt(f),
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "{}",
            match self {
                TrySendError::Full(..) => "no available capacity",
                TrySendError::Closed(..) => "channel closed",
            }
        )
    }
}

impl<T> Error for TrySendError<T> {}

impl<T> From<SendError<T>> for TrySendError<T> {
    fn from(src: SendError<T>) -> TrySendError<T> {
        TrySendError::Closed(src.0)
    }
}

/// Error returned by [`Sender::send_timeout`](super::Sender::send_timeout):
/// the value was not sent, and comes back.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum SendTimeoutError<T> {
    /// The channel stayed full until the timeout.
    Timeout(T),

    /// The receive half of the channel was explicitly closed or has been
    /// dropped.
    Closed(T),
}

impl<T> SendTimeoutError<T> {
    /// Consume the `SendTimeoutError`, returning the unsent value.
    pub fn into_inner(self) -> T {
        match self {
            SendTimeoutError::Timeout(val) => val,
            SendTimeoutError::Closed(val) => val,
        }
    }
}

impl<T> fmt::Debug for SendTimeoutError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SendTimeoutError::Timeout(..) => "Timeout(..)".fmt(f),
            SendTimeoutError::Closed(..) => "Closed(..)".fmt(f),
        }
    }
}

impl<T> fmt::Display for SendTimeoutError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.write_str(match self {
            SendTimeoutError::Timeout(..) => "timed out waiting on send operation",
            SendTimeoutError::Closed(..) => "channel closed",
        })
    }
}

impl<T> Error for SendTimeoutError<T> {}

/// Error returned by [`Receiver::try_recv`](super::Receiver::try_recv) and
/// [`UnboundedReceiver::try_recv`](super::UnboundedReceiver::try_recv).
///
/// ```
/// use nuclei::sync::mpsc::{self, error::TryRecvError};
///
/// let (tx, mut rx) = mpsc::unbounded_channel::<u8>();
/// assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
/// drop(tx);
/// assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
/// ```
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum TryRecvError {
    /// This **channel** is currently empty, but the **Sender**(s) have not yet
    /// disconnected, so data may yet become available.
    Empty,
    /// The **channel**'s sending half has become disconnected, and there will
    /// never be any more data received on it.
    Disconnected,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TryRecvError::Empty => "receiving on an empty channel".fmt(fmt),
            TryRecvError::Disconnected => "receiving on a closed channel".fmt(fmt),
        }
    }
}

impl Error for TryRecvError {}
