//! Multi-producer, single-consumer channels for the tasks of one shard.
//!
//! The names, signatures and error types follow [`sync::mpsc`](crate::sync::mpsc),
//! so code switches by changing the path. The state sits in one `Rc` and
//! plain cells, so the halves are `!Send` and no operation touches an atomic.
//!
//! The bounded flavor ([`channel`]) holds at most a fixed number of messages
//! and serves waiting sends strictly in arrival order; the unbounded one
//! ([`unbounded_channel`]) never makes a send wait.
//!
//! What one sender sends comes out in the order it was sent. When every
//! sender is dropped the stream ends: once the buffered values are received,
//! `recv` returns `None`. If the receiver is dropped, its unread values are
//! dropped with it and every later send fails with an error that holds the
//! value; [`Receiver::close`] first stops further sends and keeps the
//! buffered values, and the slots reserved before it, usable.
//!
//! Left out, because a local channel never leaves its thread: the
//! `blocking_*` methods (a blocking call there could only wait for itself)
//! and `WeakSender`.
//!
//! Receives, and sends that complete at their first attempt, spend the
//! cooperative budget (see [`task::coop`](crate::task::coop)) as the atomic
//! channels do.

/// The receiving methods both flavors share.
macro_rules! receiver_methods {
    () => {
        /// Receives the next value, waiting for one if the channel is open
        /// and empty. Answers `None` once the channel is closed and drained
        /// (no sender, and no reserved slot still to be used).
        ///
        /// Cancel safe: a dropped future loses no value.
        pub async fn recv(&mut self) -> Option<T> {
            core::future::poll_fn(|cx| self.poll_recv(cx)).await
        }

        /// Receives up to `limit` values into `buf` and answers how many,
        /// waiting for at least one. Answers 0 when `limit` is 0 or the
        /// channel ended.
        ///
        /// Cancel safe: a dropped future loses no value.
        pub async fn recv_many(&mut self, buf: &mut Vec<T>, limit: usize) -> usize {
            core::future::poll_fn(|cx| self.poll_recv_many(cx, buf, limit)).await
        }

        /// Polls to receive the next value; the waker of `cx` is woken when
        /// a value arrives or the channel ends.
        pub fn poll_recv(
            &mut self,
            cx: &mut core::task::Context<'_>,
        ) -> core::task::Poll<Option<T>> {
            self.chan.poll_recv(cx)
        }

        /// Polls to receive up to `limit` values into `buf`; see
        /// [`recv_many`](Self::recv_many).
        pub fn poll_recv_many(
            &mut self,
            cx: &mut core::task::Context<'_>,
            buf: &mut Vec<T>,
            limit: usize,
        ) -> core::task::Poll<usize> {
            self.chan.poll_recv_many(cx, buf, limit)
        }

        /// Receives the next value without waiting.
        ///
        /// # Errors
        ///
        /// [`TryRecvError::Empty`](crate::sync::mpsc::error::TryRecvError::Empty)
        /// if a value may yet come,
        /// [`TryRecvError::Disconnected`](crate::sync::mpsc::error::TryRecvError::Disconnected)
        /// if the channel ended.
        pub fn try_recv(&mut self) -> Result<T, crate::sync::mpsc::error::TryRecvError> {
            self.chan.try_recv()
        }

        /// Closes the receiving half without dropping it: no further value
        /// can be sent, and sends that wait fail. The values already
        /// buffered, and the slots reserved before the close, stay usable.
        pub fn close(&mut self) {
            self.chan.close_rx();
        }

        /// Whether the channel is closed: [`close`](Self::close) was called,
        /// or every sender is gone. Buffered values may still be received.
        pub fn is_closed(&self) -> bool {
            self.chan.rx_is_closed()
        }

        /// Whether the channel holds no value.
        pub fn is_empty(&self) -> bool {
            self.chan.len() == 0
        }

        /// How many values the channel holds.
        pub fn len(&self) -> usize {
            self.chan.len()
        }
    };
}

mod bounded;
mod chan;
mod unbounded;

pub use self::bounded::{Permit, Receiver, Sender, channel};
pub use self::unbounded::{UnboundedReceiver, UnboundedSender, unbounded_channel};
