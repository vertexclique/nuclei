//! The unbounded channel: [`unbounded_channel`], [`UnboundedSender`],
//! [`WeakUnboundedSender`] and [`UnboundedReceiver`].
//!
//! The channel's admission control is one word: the number of values in flight
//! above a closed bit. A send adds one with a compare-and-swap that refuses
//! once the closed bit is up, the receiver subtracts one per value it takes,
//! and the receiver closing sets the bit. The word is idle when no value is
//! queued or on its way, which is what a closed receiver waits for before it
//! reports the end.

use core::fmt;
use core::future::poll_fn;
use core::task::{Context, Poll};

use super::chan::{Chan, Limit};
use super::error::{SendError, TryRecvError};
use super::queue;
use crate::sync::internal::{self, Arc, AtomicUsize, Ordering};

/// Set when the receiver closed; the count of values in flight sits above it.
const CLOSED: usize = 1;
/// One value, in the units of the word.
const ONE: usize = 2;

/// The unbounded flavor's limit: values in flight, and the closed bit.
pub(super) struct Unbounded(AtomicUsize);

impl Unbounded {
    /// Admits one more value unless the channel is closed. Lock-free: the
    /// compare-and-swap retries only when another sender or the receiver
    /// changed the word first.
    fn admit(&self) -> bool {
        let mut state = self.0.load(Ordering::Acquire);
        loop {
            if state & CLOSED != 0 {
                return false;
            }
            // Cannot overflow into the closed bit: it would take 2^62 values
            // queued at once, and each one occupies at least a word of the
            // message queue's memory.
            match self.0.compare_exchange_weak(
                state,
                state + ONE,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => state = actual,
            }
        }
    }
}

impl Limit for Unbounded {
    type Queue<T> = queue::Radix<T>;

    fn queue<T>(&self) -> queue::Radix<T> {
        queue::Radix::new()
    }

    fn is_idle(&self) -> bool {
        self.0.load(Ordering::Acquire) >> 1 == 0
    }

    fn release(&self, n: usize) {
        let before = self.0.fetch_sub(n * ONE, Ordering::AcqRel);
        debug_assert!(before >> 1 >= n, "released more values than were admitted");
    }

    fn close(&self) {
        self.0.fetch_or(CLOSED, Ordering::AcqRel);
    }

    fn is_closed(&self) -> bool {
        self.0.load(Ordering::Acquire) & CLOSED != 0
    }
}

/// Creates an unbounded channel for communicating between tasks without
/// backpressure.
///
/// A send never waits and never fails for lack of room, so
/// [`UnboundedSender::send`] is an ordinary function, usable from synchronous
/// and asynchronous code alike. The queue grows with the values sent faster
/// than they are received, so memory is the only limit; use
/// [`channel`](super::channel) where a producer can outrun the receiver.
///
/// Everything sent on an [`UnboundedSender`] comes out of the
/// [`UnboundedReceiver`], in the order it was sent (see
/// [Ordering](super#ordering)). The sender can be cloned; only one receiver
/// exists.
///
/// ```
/// use nuclei::sync::mpsc;
///
/// let (tx, mut rx) = mpsc::unbounded_channel();
/// let producer = std::thread::spawn(move || {
///     for i in 0..10 {
///         tx.send(i).expect("the receiver is still there");
///     }
/// });
/// for i in 0..10 {
///     assert_eq!(rx.blocking_recv(), Some(i));
/// }
/// assert_eq!(rx.blocking_recv(), None);
/// producer.join().unwrap();
/// ```
pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>) {
    let chan = Arc::new(Chan::new(Unbounded(AtomicUsize::new(0))));
    let tx = UnboundedSender {
        chan: Arc::clone(&chan),
    };
    (tx, UnboundedReceiver { chan })
}

/// Sends values to the associated [`UnboundedReceiver`].
///
/// Created by [`unbounded_channel`]. Cloning it is a reference-count
/// increment.
///
/// # Progress
///
/// Wait-free (a bounded number of the caller's own atomic steps):
/// [`is_closed`](Self::is_closed), [`same_channel`](Self::same_channel),
/// [`strong_count`](Self::strong_count), [`weak_count`](Self::weak_count),
/// [`downgrade`](Self::downgrade), `clone` and `drop`. The drop of the last
/// sender ends with a wake of the receiver's waker cell, which is two atomic
/// steps; what the waker then does belongs to the executor.
///
/// Lock-free, not wait-free: [`send`](Self::send). It admits the value with a
/// compare-and-swap that retries when another sender or the receiver changed
/// the count first, and then pushes onto the message queue, which retries when
/// it loses a slot to another sender and allocates a segment when the last one
/// is full. Some thread's call completes whenever any retries.
/// [`closed`](Self::closed) is lock-free: a poll may queue a waiter.
pub struct UnboundedSender<T> {
    chan: Arc<Chan<T, Unbounded>>,
}

/// A sender that does not keep the channel open.
///
/// When every [`UnboundedSender`] is dropped and only `WeakUnboundedSender`s
/// remain, the channel is closed for the receiver. [`upgrade`](Self::upgrade)
/// turns it into an [`UnboundedSender`] while one exists.
///
/// ```
/// use nuclei::sync::mpsc;
///
/// let (tx, _rx) = mpsc::unbounded_channel::<i32>();
/// let weak = tx.downgrade();
/// assert!(weak.upgrade().is_some());
/// drop(tx);
/// assert!(weak.clone().upgrade().is_none());
/// ```
///
/// # Progress
///
/// `clone`, `drop`, [`strong_count`](Self::strong_count) and
/// [`weak_count`](Self::weak_count) are wait-free. [`upgrade`](Self::upgrade)
/// is lock-free, not wait-free: it is a compare-and-swap on the sender count
/// that retries when a sender was cloned or dropped meanwhile.
pub struct WeakUnboundedSender<T> {
    chan: Arc<Chan<T, Unbounded>>,
}

/// Receives values from the associated [`UnboundedSender`]s.
///
/// Created by [`unbounded_channel`]. There is exactly one receiver.
///
/// # Progress
///
/// Wait-free: [`is_closed`](Self::is_closed), [`is_empty`](Self::is_empty),
/// [`len`](Self::len), [`sender_strong_count`](Self::sender_strong_count) and
/// [`sender_weak_count`](Self::sender_weak_count): each is a load.
///
/// Lock-free: [`recv`](Self::recv), [`recv_many`](Self::recv_many),
/// [`try_recv`](Self::try_recv) and the two `poll_` forms. Registering the
/// waker is wait-free (the cell runs two read-modify-writes and no loop).
/// Taking a value off the message queue is a compare-and-swap no other thread
/// contends for, and never waits for a sender: a value whose send claimed its
/// place and has not written it yet is not there, and its sender wakes the
/// receiver once it is (see the [Progress](super#progress) notes of the
/// module). [`close`](Self::close) is lock-free: one `fetch_or` on the
/// count, then the set of the event that `closed` waits on, which takes that
/// event's drain role. The drop is the same as a receive per value left, since
/// it drops every value still queued, and so takes time proportional to them.
///
/// `blocking_recv` and `blocking_recv_many` park the calling thread until a
/// value arrives.
pub struct UnboundedReceiver<T> {
    chan: Arc<Chan<T, Unbounded>>,
}

impl<T> UnboundedSender<T> {
    /// Sends a value, without waiting.
    ///
    /// A successful send means the other end had not hung up when the value
    /// was published. `Ok` does not mean the value will be received: the
    /// receiver can hang up right after.
    ///
    /// # Errors
    ///
    /// If the receiving half is closed, by [`UnboundedReceiver::close`] or by
    /// being dropped, the function returns an error that holds the value.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::unbounded_channel();
    /// tx.send("first").unwrap();
    /// assert_eq!(rx.try_recv(), Ok("first"));
    /// drop(rx);
    /// assert_eq!(tx.send("second").unwrap_err().0, "second");
    /// ```
    pub fn send(&self, message: T) -> Result<(), SendError<T>> {
        if !self.chan.limit.admit() {
            return Err(SendError(message));
        }
        self.chan.send(message);
        Ok(())
    }

    /// Completes when the receiver has been closed or dropped.
    ///
    /// This lets a producer learn that nobody wants its values any more and
    /// stop work early.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe. Once the channel is closed it stays closed, and every later
    /// call completes at once.
    ///
    /// ```
    /// # use std::{future::Future, pin::pin, task::{Context, Poll, Waker}};
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, rx) = mpsc::unbounded_channel::<()>();
    /// let mut cx = Context::from_waker(Waker::noop());
    /// let mut closed = pin!(tx.closed());
    /// assert_eq!(closed.as_mut().poll(&mut cx), Poll::Pending);
    /// drop(rx);
    /// assert_eq!(closed.as_mut().poll(&mut cx), Poll::Ready(()));
    /// ```
    pub async fn closed(&self) {
        self.chan.closed().await;
    }

    /// Whether the channel is closed: the receiver was closed or dropped.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, rx) = mpsc::unbounded_channel::<()>();
    /// assert!(!tx.is_closed());
    /// drop(rx);
    /// assert!(tx.is_closed());
    /// ```
    pub fn is_closed(&self) -> bool {
        self.chan.is_closed()
    }

    /// Whether two senders belong to the same channel.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, _rx) = mpsc::unbounded_channel::<()>();
    /// let tx2 = tx.clone();
    /// assert!(tx.same_channel(&tx2));
    ///
    /// let (tx3, _rx3) = mpsc::unbounded_channel::<()>();
    /// assert!(!tx3.same_channel(&tx2));
    /// ```
    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.chan, &other.chan)
    }

    /// Converts the sender into a [`WeakUnboundedSender`], which does not keep
    /// the channel open. The sender itself stays.
    #[must_use = "Downgrade creates a WeakUnboundedSender without destroying the original non-weak sender."]
    pub fn downgrade(&self) -> WeakUnboundedSender<T> {
        self.chan.add_weak();
        WeakUnboundedSender {
            chan: Arc::clone(&self.chan),
        }
    }

    /// How many [`UnboundedSender`] handles exist.
    pub fn strong_count(&self) -> usize {
        self.chan.strong_count()
    }

    /// How many [`WeakUnboundedSender`] handles exist.
    pub fn weak_count(&self) -> usize {
        self.chan.weak_count()
    }
}

impl<T> Clone for UnboundedSender<T> {
    fn clone(&self) -> Self {
        self.chan.add_sender();
        UnboundedSender {
            chan: Arc::clone(&self.chan),
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

impl<T> WeakUnboundedSender<T> {
    /// Tries to turn the weak sender into an [`UnboundedSender`].
    ///
    /// Returns `Some` while at least one [`UnboundedSender`] is alive, and
    /// `None` once the last is gone.
    pub fn upgrade(&self) -> Option<UnboundedSender<T>> {
        self.chan.try_add_sender().then(|| UnboundedSender {
            chan: Arc::clone(&self.chan),
        })
    }

    /// How many [`UnboundedSender`] handles exist.
    pub fn strong_count(&self) -> usize {
        self.chan.strong_count()
    }

    /// How many [`WeakUnboundedSender`] handles exist.
    pub fn weak_count(&self) -> usize {
        self.chan.weak_count()
    }
}

impl<T> Clone for WeakUnboundedSender<T> {
    fn clone(&self) -> Self {
        self.chan.add_weak();
        WeakUnboundedSender {
            chan: Arc::clone(&self.chan),
        }
    }
}

impl<T> Drop for WeakUnboundedSender<T> {
    fn drop(&mut self) {
        self.chan.drop_weak();
    }
}

impl<T> fmt::Debug for WeakUnboundedSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WeakUnboundedSender").finish()
    }
}

impl<T> UnboundedReceiver<T> {
    /// Receives the next value.
    ///
    /// Returns `None` once the channel is closed and no buffered value is
    /// left, which means no value can ever be received again. The channel is
    /// closed when every sender has been dropped, or when
    /// [`close`](Self::close) is called. If the channel is empty and open, it
    /// waits for a send or a close.
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
    /// let (tx, mut rx) = mpsc::unbounded_channel();
    /// tx.send("hello").unwrap();
    /// tx.send("world").unwrap();
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
    /// let (tx, mut rx) = mpsc::unbounded_channel();
    /// for word in ["first", "second", "third"] {
    ///     tx.send(word).unwrap();
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
    /// [`TryRecvError::Empty`] if the channel is empty but a sender still
    /// exists, so a value may yet come. [`TryRecvError::Disconnected`] if it
    /// is empty and nothing can send: every sender is gone, or the channel was
    /// closed.
    ///
    /// A value whose send is still in progress on another thread is not in the
    /// channel yet, nor are the values sent after it, which come out behind
    /// it. This call does not wait for them: the sender wakes the receiver
    /// once its value is in.
    ///
    /// ```
    /// use nuclei::sync::mpsc::{self, error::TryRecvError};
    ///
    /// let (tx, mut rx) = mpsc::unbounded_channel();
    /// tx.send("hello").unwrap();
    /// assert_eq!(rx.try_recv(), Ok("hello"));
    /// assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    /// drop(tx);
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
    /// blocks that task's thread.
    ///
    /// # Panics
    ///
    /// On `wasm32`, where no other thread can send, when the channel is empty
    /// and open: the call could never end.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::unbounded_channel::<u8>();
    /// let receiver = std::thread::spawn(move || {
    ///     assert_eq!(rx.blocking_recv(), Some(10));
    ///     assert_eq!(rx.blocking_recv(), None);
    /// });
    /// tx.send(10).unwrap();
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
    /// No further value can be sent: a send fails and returns its value. The
    /// values already buffered stay receivable, so a clean shutdown closes,
    /// then calls [`recv`](Self::recv) until it returns `None`. A send that
    /// was already under way when the channel closed still completes and is
    /// received first.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::unbounded_channel();
    /// tx.send(1).unwrap();
    /// rx.close();
    /// assert_eq!(tx.send(2).unwrap_err().0, 2);
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
    /// let (tx, mut rx) = mpsc::unbounded_channel::<()>();
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
    /// let (tx, rx) = mpsc::unbounded_channel();
    /// assert!(rx.is_empty());
    /// tx.send(()).unwrap();
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
    /// let (tx, rx) = mpsc::unbounded_channel();
    /// assert_eq!(rx.len(), 0);
    /// tx.send(()).unwrap();
    /// assert_eq!(rx.len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        self.chan.len()
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

    /// How many [`UnboundedSender`] handles exist.
    pub fn sender_strong_count(&self) -> usize {
        self.chan.strong_count()
    }

    /// How many [`WeakUnboundedSender`] handles exist.
    pub fn sender_weak_count(&self) -> usize {
        self.chan.weak_count()
    }
}

impl<T> Drop for UnboundedReceiver<T> {
    fn drop(&mut self) {
        // SAFETY: the channel's one receiver, dropping.
        unsafe { self.chan.drop_rx() };
    }
}

impl<T> fmt::Debug for UnboundedReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnboundedReceiver")
            .field("closed", &self.is_closed())
            .field("len", &self.len())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::Polled;

    #[test]
    fn the_word_counts_values_in_flight_above_the_closed_bit() {
        let limit = Unbounded(AtomicUsize::new(0));
        assert!(limit.is_idle() && !limit.is_closed());
        assert!(limit.admit() && limit.admit());
        assert!(!limit.is_idle());
        limit.release(1);
        assert!(!limit.is_idle());
        limit.close();
        limit.close();
        assert!(limit.is_closed());
        assert!(!limit.admit(), "a closed channel admits nothing");
        limit.release(1);
        assert!(limit.is_idle() && limit.is_closed());
    }

    #[test]
    fn a_receiver_waiting_on_a_closed_channel_ends_when_the_last_value_is_taken() {
        let (tx, mut rx) = unbounded_channel::<u8>();
        tx.send(1).unwrap();
        rx.close();
        assert_eq!(rx.try_recv(), Ok(1));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
        let mut recv = Polled::new(rx.recv());
        assert_eq!(recv.ready(), None);
    }
}
