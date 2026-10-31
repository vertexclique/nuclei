//! The state both flavors of the single-shard channel share: the queue, the
//! receiver's waker, the sender count and the list of `closed()` waits.
//!
//! Every access to the queue is one step that runs no user code. A waker is
//! woken, and a value dropped, only after the step that took it has ended.

#![allow(unsafe_code)]

use core::cell::{Cell, UnsafeCell};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::collections::VecDeque;

use crate::sync::local::waiters::{Key, State, Waiters};
use crate::sync::mpsc::error::TryRecvError;

/// What bounds a channel: the bounded flavor counts slots, the unbounded one
/// has none.
pub(super) trait Limit {
    /// `n` values left the queue (or `n` reserved slots were given up).
    fn release(&self, n: usize);
    /// Whether no slot is reserved and still unused.
    fn is_idle(&self) -> bool;
    /// The receiver closed: stop admitting.
    fn close(&self);
}

pub(super) struct Chan<T, L> {
    queue: UnsafeCell<VecDeque<T>>,
    rx_waker: Cell<Option<Waker>>,
    senders: Cell<usize>,
    rx_closed: Cell<bool>,
    closed_waiters: Waiters,
    pub(super) limit: L,
}

impl<T, L: Limit> Chan<T, L> {
    pub(super) fn new(limit: L) -> Self {
        Self {
            queue: UnsafeCell::new(VecDeque::new()),
            rx_waker: Cell::new(None),
            senders: Cell::new(1),
            rx_closed: Cell::new(false),
            closed_waiters: Waiters::new(),
            limit,
        }
    }

    /// Runs one step on the queue.
    #[inline]
    fn queue<R>(&self, f: impl FnOnce(&mut VecDeque<T>) -> R) -> R {
        // SAFETY: the channel is `!Sync` (it holds a `Waiters`), and every
        // step is a plain queue operation that runs no user code and calls no
        // other step, so this is the only live borrow.
        f(unsafe { &mut *self.queue.get() })
    }

    pub(super) fn len(&self) -> usize {
        self.queue(|q| q.len())
    }

    /// Appends a value and wakes the receiver.
    pub(super) fn push(&self, value: T) {
        self.queue(|q| q.push_back(value));
        self.wake_rx();
    }

    fn pop(&self) -> Option<T> {
        self.queue(|q| q.pop_front())
    }

    /// Moves up to `limit` values into `buf`; answers how many.
    fn take_many(&self, buf: &mut Vec<T>, limit: usize) -> usize {
        self.queue(|q| {
            let n = q.len().min(limit);
            buf.extend(q.drain(..n));
            n
        })
    }

    pub(super) fn wake_rx(&self) {
        if let Some(waker) = self.rx_waker.take() {
            waker.wake();
        }
    }

    fn register_rx(&self, waker: &Waker) {
        match self.rx_waker.take() {
            Some(old) if old.will_wake(waker) => self.rx_waker.set(Some(old)),
            old => {
                self.rx_waker.set(Some(waker.clone()));
                drop(old);
            }
        }
    }

    pub(super) fn add_sender(&self) {
        self.senders.set(self.senders.get() + 1);
    }

    /// A sender is gone; the last one ends the stream for the receiver.
    pub(super) fn drop_sender(&self) {
        let left = self.senders.get() - 1;
        self.senders.set(left);
        if left == 0 {
            self.wake_rx();
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.rx_closed.get()
    }

    /// Whether the receiver closed or every sender is gone.
    pub(super) fn rx_is_closed(&self) -> bool {
        self.rx_closed.get() || self.senders.get() == 0
    }

    /// Whether the receiver is done: nothing is queued, nothing can be sent
    /// and no reserved slot is still to be used.
    fn finished(&self) -> bool {
        self.rx_is_closed() && self.len() == 0 && self.limit.is_idle()
    }

    /// Gives back `n` reserved slots that were never used, and wakes the
    /// receiver if that left a closed channel idle.
    pub(super) fn give_back(&self, n: usize) {
        self.limit.release(n);
        self.wake_if_finished();
    }

    pub(super) fn wake_if_finished(&self) {
        if self.finished() {
            self.wake_rx();
        }
    }

    /// The receiver's poll: a value, the end of the stream, or its waker
    /// registered. A receive that completes spends one operation of the
    /// poll's budget.
    pub(super) fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if crate::coop::exhausted(cx.waker()) {
            return Poll::Pending;
        }
        if let Some(value) = self.pop() {
            self.limit.release(1);
            crate::coop::spent_one();
            return Poll::Ready(Some(value));
        }
        if self.finished() {
            crate::coop::spent_one();
            return Poll::Ready(None);
        }
        self.register_rx(cx.waker());
        Poll::Pending
    }

    /// As [`poll_recv`](Self::poll_recv), for a batch of up to `limit`.
    pub(super) fn poll_recv_many(
        &self,
        cx: &mut Context<'_>,
        buf: &mut Vec<T>,
        limit: usize,
    ) -> Poll<usize> {
        if limit == 0 {
            return Poll::Ready(0);
        }
        if crate::coop::exhausted(cx.waker()) {
            return Poll::Pending;
        }
        let taken = self.take_many(buf, limit);
        if taken > 0 {
            self.limit.release(taken);
            crate::coop::spent_one();
            return Poll::Ready(taken);
        }
        if self.finished() {
            crate::coop::spent_one();
            return Poll::Ready(0);
        }
        self.register_rx(cx.waker());
        Poll::Pending
    }

    pub(super) fn try_recv(&self) -> Result<T, TryRecvError> {
        if let Some(value) = self.pop() {
            self.limit.release(1);
            return Ok(value);
        }
        if self.finished() {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Closes the channel to new sends and tells every `closed()` wait.
    pub(super) fn close_rx(&self) {
        if self.rx_closed.replace(true) {
            return;
        }
        self.limit.close();
        while let Some(waker) = self.closed_waiters.close_front() {
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    /// What dropping the receiver does: close, let go of its waker so the
    /// task is not kept alive, and drop every value still queued.
    pub(super) fn drop_rx(&self) {
        self.close_rx();
        drop(self.rx_waker.take());
        let left = self.queue(core::mem::take);
        self.limit.release(left.len());
        drop(left);
    }

    /// Waits until the receiver closes or is dropped.
    pub(super) fn closed(&self) -> ClosedWait<'_, T, L> {
        ClosedWait {
            chan: self,
            key: None,
        }
    }
}

/// The future of `Sender::closed`; cancel safe, and holds no place once
/// dropped.
pub(super) struct ClosedWait<'a, T, L: Limit> {
    chan: &'a Chan<T, L>,
    key: Option<Key>,
}

impl<T, L: Limit> Future for ClosedWait<'_, T, L> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let waiters = &this.chan.closed_waiters;
        if this.chan.rx_closed.get() {
            if let Some(key) = this.key.take() {
                drop(waiters.cancel(key));
            }
            return Poll::Ready(());
        }
        if let Some(key) = this.key.take() {
            if waiters.refresh(key, cx.waker()) == State::Waiting {
                this.key = Some(key);
                return Poll::Pending;
            }
            drop(waiters.cancel(key));
        }
        this.key = Some(waiters.push(0, cx.waker().clone()));
        Poll::Pending
    }
}

impl<T, L: Limit> Drop for ClosedWait<'_, T, L> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            drop(self.chan.closed_waiters.cancel(key));
        }
    }
}
