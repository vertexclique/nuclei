//! The state both channel flavors share, and every protocol step the public
//! types are thin wrappers around.
//!
//! One [`Chan`] holds the message queue, the single receiver's waker slot, the
//! sender and weak-sender counts, and a [`Limit`]: the flavor's admission
//! control and its closed bit. The bounded flavor's limit is a semaphore of
//! permits, the unbounded flavor's a counter of values in flight.
//!
//! # Invariants
//!
//! - **Admission before push.** A value is pushed only after the limit
//!   admitted it, and the limit gets it back (`release`) when the receiver
//!   takes the value or the reservation is dropped unused. So the limit is
//!   idle exactly when no value is queued, in flight or reserved.
//! - **Close, then drain.** The receiver closing the channel closes the limit
//!   first, so nothing new is admitted, and sets the `rx_closed` event so
//!   `Sender::closed` completes. A value admitted before the close is still
//!   pushed and still received: the receiver treats a closed channel as
//!   finished only once the limit is idle.
//! - **The last sender ends the channel.** The sender count reaches zero after
//!   every sender's pushes, through a chain of release/acquire operations on
//!   the count, so a receiver that read zero and then finds the queue empty
//!   has seen every value ever sent.
//! - **No lost wakeup.** The receiver registers its waker, then looks at the
//!   queue, the sender count and the limit again before it returns `Pending`.
//!   Everything that can change an answer (a push, the last sender's drop, an
//!   idle limit that is closed) wakes the registered waker after it changed
//!   the state. The waker cell is a handshake on one state word, so for any
//!   pair of a registration and a wake one of them sees the other: see the
//!   `atomic_waker` module.
//! - **Single consumer.** Only the [`Receiver`](super::Receiver) halves call
//!   the receive-side methods, each through `&mut self`, so there is one
//!   registrant at a time.
//!
//! # Why this file has no `unsafe`
//!
//! The queue is multi-consumer safe, the waker cell takes care of its own
//! slot, and the counts are atomics, so none of the state here relies on a
//! rule the compiler cannot check. The auto traits follow the value: a
//! channel is `Send + Sync` when `T: Send`.

use core::task::{Context, Poll};
use std::panic::{RefUnwindSafe, UnwindSafe};

use super::error::TryRecvError;
use super::queue::Queue;
use crate::sync::Event;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicUsize, Ordering};

/// A channel flavor's admission control and closed bit.
pub(super) trait Limit {
    /// Whether every admitted value has been received and no reservation is
    /// outstanding.
    fn is_idle(&self) -> bool;

    /// Gives back `n` admissions: `n` values were received, or `n`
    /// reservations were dropped unused.
    fn release(&self, n: usize);

    /// Stops admitting. Closing twice does nothing.
    fn close(&self);

    /// Whether [`close`](Self::close) was called.
    fn is_closed(&self) -> bool;
}

/// What looking at the queue found.
enum Pop<T> {
    Value(T),
    /// Empty, and every sender is gone: nothing will ever come.
    Closed,
    /// Empty for now.
    Empty,
}

/// The shared state of one channel.
pub(super) struct Chan<T: 'static, L> {
    queue: Queue<T>,
    /// The receiver's task, woken when a value arrives or the channel ends.
    rx_waker: AtomicWaker,
    /// Set when the receiver closes or is dropped; `Sender::closed` waits on it.
    rx_closed: Event,
    pub(super) limit: L,
    /// How many `Sender`s (and owned permits) exist. Zero ends the channel.
    tx_count: AtomicUsize,
    /// How many `WeakSender`s exist.
    tx_weak_count: AtomicUsize,
}

// A channel holds no invariant that a panic in a caller's closure could break
// half way: every shared word is an atomic and every step is complete before
// the next, so a panic leaves it usable.
impl<T: 'static, L> UnwindSafe for Chan<T, L> {}
impl<T: 'static, L> RefUnwindSafe for Chan<T, L> {}

impl<T: 'static, L: Limit> Chan<T, L> {
    /// A channel with one sender counted.
    pub(super) fn new(limit: L) -> Self {
        Self {
            queue: Queue::new(),
            rx_waker: AtomicWaker::new(),
            rx_closed: Event::new(),
            limit,
            tx_count: AtomicUsize::new(1),
            tx_weak_count: AtomicUsize::new(0),
        }
    }

    // ===== the sending side =====

    /// Pushes a value the limit admitted, then wakes the receiver.
    ///
    /// The value is in the queue before the waker is touched, so a panic in
    /// the receiver's waker leaves the channel consistent.
    pub(super) fn send(&self, value: T) {
        self.queue.push(value);
        self.rx_waker.wake();
    }

    /// Gives back `n` admissions that were reserved and never used, and wakes
    /// the receiver if that left a closed channel idle (the receiver then
    /// finishes).
    pub(super) fn give_back(&self, n: usize) {
        self.limit.release(n);
        self.wake_if_finished();
    }

    /// Wakes the receiver when the channel is closed and idle. A reservation
    /// that ends without a `give_back` (a cancelled wait that was handed
    /// permits) calls this after its permits are back.
    pub(super) fn wake_if_finished(&self) {
        if self.finished() {
            self.rx_waker.wake();
        }
    }

    /// Whether the receiver closed or was dropped.
    pub(super) fn is_closed(&self) -> bool {
        self.limit.is_closed()
    }

    /// Completes when the receiver closes or is dropped.
    pub(super) async fn closed(&self) {
        if self.limit.is_closed() {
            return;
        }
        self.rx_closed.wait().await;
    }

    /// Counts one more sender. The caller holds a sender, so the count cannot
    /// fall to zero meanwhile and a relaxed add is enough.
    pub(super) fn add_sender(&self) {
        self.tx_count.fetch_add(1, Ordering::Relaxed);
    }

    /// A sender is gone; the last one ends the channel for the receiver.
    pub(super) fn drop_sender(&self) {
        // AcqRel: the release publishes this sender's pushes through the
        // count; the acquire makes the last sender see all earlier ones'.
        if self.tx_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.rx_waker.wake();
        }
    }

    /// Counts one more weak sender.
    pub(super) fn add_weak(&self) {
        self.tx_weak_count.fetch_add(1, Ordering::Relaxed);
    }

    /// A weak sender is gone.
    pub(super) fn drop_weak(&self) {
        self.tx_weak_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Counts a new sender unless the last one is gone. Returns whether it
    /// did. Lock-free: it retries when another call changed the count first.
    pub(super) fn try_add_sender(&self) -> bool {
        let mut count = self.tx_count.load(Ordering::Acquire);
        loop {
            if count == 0 {
                return false;
            }
            match self.tx_count.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => count = actual,
            }
        }
    }

    /// How many senders exist.
    pub(super) fn strong_count(&self) -> usize {
        self.tx_count.load(Ordering::Acquire)
    }

    /// How many weak senders exist.
    pub(super) fn weak_count(&self) -> usize {
        self.tx_weak_count.load(Ordering::Relaxed)
    }

    // ===== the receiving side =====

    /// Whether the channel is closed: the receiver closed it, or every sender
    /// is gone.
    pub(super) fn rx_is_closed(&self) -> bool {
        self.limit.is_closed() || self.tx_count.load(Ordering::Acquire) == 0
    }

    /// How many values are queued, as a snapshot.
    pub(super) fn len(&self) -> usize {
        self.queue.len()
    }

    /// Looks at the queue once.
    fn pop(&self) -> Pop<T> {
        if let Some(value) = self.queue.pop() {
            return Pop::Value(value);
        }
        // Count first, then look again: the count reads zero only after every
        // sender's pushes, so an empty queue behind a zero count is final.
        if self.tx_count.load(Ordering::Acquire) != 0 {
            return Pop::Empty;
        }
        match self.queue.pop() {
            Some(value) => Pop::Value(value),
            None => Pop::Closed,
        }
    }

    /// One value, or the end of the channel, if either is there. A value
    /// taken here is given back to the limit.
    fn next(&self) -> Option<Option<T>> {
        match self.pop() {
            Pop::Value(value) => {
                self.limit.release(1);
                Some(Some(value))
            }
            Pop::Closed => Some(None),
            Pop::Empty => None,
        }
    }

    /// Whether a receiver that found the queue empty is done: the channel was
    /// closed and nothing admitted is still on its way.
    fn finished(&self) -> bool {
        self.limit.is_closed() && self.limit.is_idle()
    }

    /// The receiver's poll: register, then look again, then decide.
    pub(super) fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if let Some(done) = self.next() {
            return Poll::Ready(done);
        }
        self.rx_waker.register(cx.waker());
        // A value pushed, or a last sender dropped, between the look above
        // and the registration is found here, or woke the waker.
        if let Some(done) = self.next() {
            return Poll::Ready(done);
        }
        if self.finished() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    /// Moves up to `limit` values into `buf`. `Some(n)` when it moved `n > 0`
    /// of them or the channel has ended (`n == 0`), `None` when it is empty
    /// for now. One release to the limit for the whole batch.
    fn take_many(&self, buf: &mut Vec<T>, limit: usize) -> Option<usize> {
        let mut taken = 0;
        let ended = loop {
            if taken == limit {
                break false;
            }
            match self.pop() {
                Pop::Value(value) => {
                    buf.push(value);
                    taken += 1;
                }
                Pop::Closed => break true,
                Pop::Empty => break false,
            }
        };
        if taken > 0 {
            self.limit.release(taken);
        }
        (taken > 0 || ended).then_some(taken)
    }

    /// The receiver's batch poll; see [`take_many`](Self::take_many).
    pub(super) fn poll_recv_many(
        &self,
        cx: &mut Context<'_>,
        buf: &mut Vec<T>,
        limit: usize,
    ) -> Poll<usize> {
        if limit == 0 {
            return Poll::Ready(0);
        }
        if let Some(taken) = self.take_many(buf, limit) {
            return Poll::Ready(taken);
        }
        self.rx_waker.register(cx.waker());
        if let Some(taken) = self.take_many(buf, limit) {
            return Poll::Ready(taken);
        }
        if self.finished() {
            Poll::Ready(0)
        } else {
            Poll::Pending
        }
    }

    /// The receiver's non-waiting look.
    ///
    /// It does not park for a send that is still in progress on another
    /// thread: that value is not in the channel yet, and its sender wakes the
    /// receiver when it is. (The queue's own spin, see the `queue` module, is
    /// the one wait.)
    pub(super) fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.pop() {
            Pop::Value(value) => {
                self.limit.release(1);
                Ok(value)
            }
            Pop::Closed => Err(TryRecvError::Disconnected),
            Pop::Empty if self.finished() => Err(TryRecvError::Disconnected),
            Pop::Empty => Err(TryRecvError::Empty),
        }
    }

    /// Closes the channel to new sends and tells every `Sender::closed`.
    pub(super) fn close_rx(&self) {
        self.limit.close();
        self.rx_closed.set();
    }

    /// Drops every queued value, giving each one's admission back. A value
    /// whose destructor panics is dropped after its admission came back, and
    /// the `Drain` guard of [`drop_rx`](Self::drop_rx) carries on with the rest.
    fn drain(&self) {
        while let Some(value) = self.queue.pop() {
            self.limit.release(1);
            drop(value);
        }
    }

    /// What dropping the receiver does: close, let go of its waker so the
    /// task is not kept alive, and drop everything still queued.
    pub(super) fn drop_rx(&self) {
        /// Finishes the drain when a value's destructor panics.
        struct Drain<'a, T: 'static, L: Limit>(&'a Chan<T, L>);

        impl<T: 'static, L: Limit> Drop for Drain<'_, T, L> {
            fn drop(&mut self) {
                self.0.drain();
            }
        }

        self.close_rx();
        drop(self.rx_waker.take());
        let guard = Drain(self);
        guard.0.drain();
    }
}
