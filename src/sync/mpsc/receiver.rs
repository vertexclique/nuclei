//! The receiving half of the bounded channel.

use core::fmt;
use core::future::poll_fn;
use core::task::{Context, Poll};

use super::bounded::Bounded;
use super::chan::Chan;
use super::error::TryRecvError;
use crate::sync::internal::{self, Arc};

/// Receives values from the associated [`Sender`](super::Sender)s.
///
/// Created by [`channel`](super::channel). There is exactly one receiver.
///
/// # Progress
///
/// Wait-free: [`is_closed`](Self::is_closed), [`is_empty`](Self::is_empty),
/// [`len`](Self::len), [`capacity`](Self::capacity),
/// [`max_capacity`](Self::max_capacity),
/// [`sender_strong_count`](Self::sender_strong_count) and
/// [`sender_weak_count`](Self::sender_weak_count): each is a load.
///
/// Lock-free: [`recv`](Self::recv), [`recv_many`](Self::recv_many),
/// [`try_recv`](Self::try_recv) and the two `poll_` forms. Registering the
/// waker is wait-free (the cell runs two read-modify-writes and no loop).
/// Taking a value off the message queue is a compare-and-swap no other thread
/// contends for, and never waits for a sender: a value whose send claimed its
/// place and has not written it yet is not there, and its sender wakes the
/// receiver once it is (see the [Progress](super#progress) notes of the
/// module). Giving the slot back to the senders is a semaphore release
/// that, when senders wait, serves the queue and is lock-free.
/// [`close`](Self::close) is lock-free; the drop is the same as a receive per
/// value left, since it drops every value still queued, and so takes time
/// proportional to them.
///
/// `blocking_recv` and `blocking_recv_many` park the calling thread until a
/// value arrives.
pub struct Receiver<T> {
    chan: Arc<Chan<T, Bounded>>,
}

impl<T> Receiver<T> {
    pub(super) fn new(chan: Arc<Chan<T, Bounded>>) -> Self {
        Self { chan }
    }

    /// Receives the next value.
    ///
    /// Returns `None` once the channel is closed and no buffered value is
    /// left, which means no value can ever be received again. The channel is
    /// closed when every sender has been dropped, or when
    /// [`close`](Self::close) is called. If the buffer is empty and the
    /// channel is open, it waits for a send or a close.
    ///
    /// If [`close`](Self::close) was called while a [`Permit`](super::Permit)
    /// or an [`OwnedPermit`](super::OwnedPermit) was still held, the channel
    /// does not count as finished for `recv` until the permit is sent or
    /// dropped.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe. If the future is dropped (for instance when another branch
    /// of a `select` wins), no value was taken from the channel.
    ///
    /// ```
    /// # use std::{future::Future, pin::pin, task::{Context, Poll, Waker}};
    /// # fn now<F: Future>(f: F) -> F::Output {
    /// #     match pin!(f).poll(&mut Context::from_waker(Waker::noop())) {
    /// #         Poll::Ready(out) => out,
    /// #         Poll::Pending => panic!("not ready"),
    /// #     }
    /// # }
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel(100);
    /// tx.try_send("hello").unwrap();
    /// tx.try_send("world").unwrap();
    /// drop(tx);
    /// assert_eq!(now(rx.recv()), Some("hello"));
    /// assert_eq!(now(rx.recv()), Some("world"));
    /// assert_eq!(now(rx.recv()), None);
    /// ```
    pub async fn recv(&mut self) -> Option<T> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Receives the next values and appends them to `buffer`, taking at most
    /// `limit` of them in one call.
    ///
    /// Returns how many values it added. A `limit` of zero returns `0` at
    /// once. Otherwise it waits like [`recv`](Self::recv) until at least one
    /// value is there, then takes everything that is ready up to `limit`
    /// without waiting again, and returns `0` only when the channel is closed
    /// and empty. The capacity of `buffer` grows as needed.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe, as [`recv`](Self::recv): a dropped future took nothing.
    ///
    /// ```
    /// # use std::{future::Future, pin::pin, task::{Context, Poll, Waker}};
    /// # fn now<F: Future>(f: F) -> F::Output {
    /// #     match pin!(f).poll(&mut Context::from_waker(Waker::noop())) {
    /// #         Poll::Ready(out) => out,
    /// #         Poll::Pending => panic!("not ready"),
    /// #     }
    /// # }
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel(100);
    /// for word in ["first", "second", "third"] {
    ///     tx.try_send(word).unwrap();
    /// }
    /// let mut buffer = Vec::new();
    /// assert_eq!(now(rx.recv_many(&mut buffer, 2)), 2);
    /// assert_eq!(buffer, ["first", "second"]);
    /// assert_eq!(now(rx.recv_many(&mut buffer, 2)), 1);
    /// drop(tx);
    /// assert_eq!(now(rx.recv_many(&mut buffer, 2)), 0);
    /// ```
    pub async fn recv_many(&mut self, buffer: &mut Vec<T>, limit: usize) -> usize {
        poll_fn(|cx| self.poll_recv_many(cx, buffer, limit)).await
    }

    /// Tries to receive the next value without waiting.
    ///
    /// # Errors
    ///
    /// [`TryRecvError::Empty`] if the channel is empty but a sender or a
    /// permit still exists, so a value may yet come.
    /// [`TryRecvError::Disconnected`] if it is empty and nothing can send:
    /// every sender is gone, or the channel was closed and no permit is held.
    ///
    /// A value whose send is still in progress on another thread is not in the
    /// channel yet, nor are the values sent after it, which come out behind
    /// it. This call does not wait for them: the sender wakes the receiver
    /// once its value is in.
    ///
    /// ```
    /// use nuclei::sync::mpsc::{self, error::TryRecvError};
    ///
    /// let (tx, mut rx) = mpsc::channel(100);
    /// tx.try_send("hello").unwrap();
    /// assert_eq!(rx.try_recv(), Ok("hello"));
    /// assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    ///
    /// tx.try_send("hello").unwrap();
    /// drop(tx);
    /// assert_eq!(rx.try_recv(), Ok("hello"));
    /// assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    /// ```
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        // SAFETY: `&mut self` of the channel's one receiver.
        unsafe { self.chan.try_recv() }
    }

    /// Receives the next value from a thread that may block.
    ///
    /// Like [`recv`](Self::recv), but it parks the calling thread while the
    /// channel is empty and open. On a nuclei shard (inside one of its tasks) it does not park the thread: it runs a nested `block_on`, so the shard keeps running its other tasks meanwhile. Inside another runtime's task it
    /// blocks that task's thread. It works whether or not the senders use
    /// [`Sender::blocking_send`](super::Sender::blocking_send).
    ///
    /// # Panics
    ///
    /// On `wasm32`, where no other thread can send, when the channel is empty
    /// and open: the call could never end.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel::<u8>(10);
    /// let receiver = std::thread::spawn(move || {
    ///     assert_eq!(rx.blocking_recv(), Some(10));
    ///     assert_eq!(rx.blocking_recv(), None);
    /// });
    /// tx.blocking_send(10).unwrap();
    /// drop(tx);
    /// receiver.join().unwrap();
    /// ```
    pub fn blocking_recv(&mut self) -> Option<T> {
        internal::blocking(self.recv())
    }

    /// [`recv_many`](Self::recv_many) for a thread that may block; see
    /// [`blocking_recv`](Self::blocking_recv).
    ///
    /// # Panics
    ///
    /// As for [`blocking_recv`](Self::blocking_recv).
    pub fn blocking_recv_many(&mut self, buffer: &mut Vec<T>, limit: usize) -> usize {
        internal::blocking(self.recv_many(buffer, limit))
    }

    /// Closes the receiving half without dropping it.
    ///
    /// No further value can be sent: a send fails and returns its value. A
    /// slot reserved before the close can still be used, and the values
    /// already buffered stay receivable, so a clean shutdown closes, then
    /// calls [`recv`](Self::recv) until it returns `None`. That happens once
    /// every reserved slot is sent or dropped.
    ///
    /// ```
    /// use nuclei::sync::mpsc::{self, error::TrySendError};
    ///
    /// let (tx, mut rx) = mpsc::channel(20);
    /// tx.try_send(1).unwrap();
    /// rx.close();
    /// assert!(matches!(tx.try_send(2), Err(TrySendError::Closed(2))));
    /// assert_eq!(rx.try_recv(), Ok(1), "the buffered value is still there");
    /// ```
    pub fn close(&mut self) {
        self.chan.close_rx();
    }

    /// Whether the channel is closed: [`close`](Self::close) was called, or
    /// every sender is gone. Buffered values may still be received.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel::<()>(10);
    /// assert!(!rx.is_closed());
    /// rx.close();
    /// assert!(rx.is_closed());
    /// drop(tx);
    /// ```
    pub fn is_closed(&self) -> bool {
        self.chan.rx_is_closed()
    }

    /// Whether the channel holds no value.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, rx) = mpsc::channel(10);
    /// assert!(rx.is_empty());
    /// tx.try_send(0).unwrap();
    /// assert!(!rx.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.chan.len() == 0
    }

    /// How many values the channel holds.
    ///
    /// A snapshot: it is exact while no other thread is sending, and while one
    /// is it may be off by the values being sent at that moment.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, rx) = mpsc::channel(10);
    /// assert_eq!(rx.len(), 0);
    /// tx.try_send(0).unwrap();
    /// assert_eq!(rx.len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        self.chan.len()
    }

    /// The channel's current capacity: the slots that are free.
    ///
    /// It goes down when a value is sent or a slot reserved, and up when a
    /// value is received or a permit dropped unused. Compare
    /// [`max_capacity`](Self::max_capacity), which never changes.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel::<()>(5);
    /// assert_eq!(rx.capacity(), 5);
    /// let permit = tx.try_reserve().unwrap();
    /// assert_eq!(rx.capacity(), 4);
    /// assert_eq!(rx.len(), 0);
    /// permit.send(());
    /// assert_eq!(rx.len(), 1);
    /// rx.try_recv().unwrap();
    /// assert_eq!(rx.capacity(), 5);
    /// ```
    pub fn capacity(&self) -> usize {
        self.chan.limit.sem.available()
    }

    /// The channel's maximum capacity: the `buffer` it was created with.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, rx) = mpsc::channel::<()>(5);
    /// let _permit = tx.try_reserve().unwrap();
    /// assert_eq!(rx.max_capacity(), 5);
    /// assert_eq!(rx.capacity(), 4);
    /// ```
    pub fn max_capacity(&self) -> usize {
        self.chan.limit.bound
    }

    /// Polls to receive the next value.
    ///
    /// Returns [`Poll::Pending`] if no value is available and the channel is
    /// open; [`Poll::Ready`]`(Some(value))` if one is; and
    /// [`Poll::Ready`]`(None)` if the channel is closed and every value sent
    /// before it closed was received.
    ///
    /// On `Pending`, the waker of `cx` is woken when a value arrives or the
    /// channel closes. Only the waker of the most recent call to `poll_recv`
    /// or [`poll_recv_many`](Self::poll_recv_many) is kept.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        // SAFETY: `&mut self` of the channel's one receiver.
        unsafe { self.chan.poll_recv(cx) }
    }

    /// Polls to receive several values into `buffer`, at most `limit`.
    ///
    /// Returns [`Poll::Pending`] if no value is available and the channel is
    /// open; [`Poll::Ready`]`(count)` with the number of values added, which
    /// is at least one unless `limit` is zero or the channel is closed and
    /// empty. It takes everything that is ready, up to `limit`, in one call.
    /// The waker of `cx` is handled as for [`poll_recv`](Self::poll_recv).
    pub fn poll_recv_many(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut Vec<T>,
        limit: usize,
    ) -> Poll<usize> {
        // SAFETY: `&mut self` of the channel's one receiver.
        unsafe { self.chan.poll_recv_many(cx, buffer, limit) }
    }

    /// How many [`Sender`](super::Sender) handles exist.
    pub fn sender_strong_count(&self) -> usize {
        self.chan.strong_count()
    }

    /// How many [`WeakSender`](super::WeakSender) handles exist.
    pub fn sender_weak_count(&self) -> usize {
        self.chan.weak_count()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // SAFETY: the channel's one receiver, dropping.
        unsafe { self.chan.drop_rx() };
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("closed", &self.is_closed())
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .field("max_capacity", &self.max_capacity())
            .finish()
    }
}
