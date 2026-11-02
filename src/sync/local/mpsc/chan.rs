//! The state both flavors of the single-shard channel share: the queue, the
//! receiver's waker, the sender count and the list of `closed()` waits.
//!
//! Every access to the queue is one step that runs no user code. A waker is
//! woken, and a value dropped, only after the step that took it has ended.

#![allow(unsafe_code)]

use core::cell::{Cell, UnsafeCell};
use core::future::Future;
use core::mem::{self, ManuallyDrop};
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::collections::VecDeque;

use crate::sync::local::waiters::{CloseRest, Key, State, Waiters, wake_closed};
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
    /// Takes back `n` admissions a release handed on, as a debt the next
    /// release pays.
    fn debit(&self, n: usize);
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

    /// Makes `waker` the receiver's. Answers whether it cloned, which ran
    /// user code that may have sent or closed: the caller looks again.
    fn register_rx(&self, waker: &Waker) -> bool {
        match self.rx_waker.take() {
            Some(old) if old.will_wake(waker) => {
                self.rx_waker.set(Some(old));
                false
            }
            old => {
                self.rx_waker.set(Some(waker.clone()));
                drop(old);
                true
            }
        }
    }

    /// Pops a value and gives its slot back, which wakes a waiting sender.
    /// A sender's waker that panics unwinds the release: the guard puts the
    /// value back at the front and takes back the admission the release
    /// handed on, so the value comes out again exactly once.
    #[inline(always)]
    fn pop_released(&self) -> Option<T> {
        let value = ManuallyDrop::new(self.pop()?);
        let restore = Restore {
            chan: self,
            value: &value,
        };
        self.limit.release(1);
        mem::forget(restore);
        Some(ManuallyDrop::into_inner(value))
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
        if let Some(value) = self.pop_released() {
            crate::coop::spent_one();
            return Poll::Ready(Some(value));
        }
        // The waker is registered before the end is looked at, so one look
        // serves both: a clone is user code that may have sent or closed.
        if self.register_rx(cx.waker()) && self.len() != 0 {
            return self.look_again();
        }
        if self.finished() {
            drop(self.rx_waker.take());
            crate::coop::spent_one();
            return Poll::Ready(None);
        }
        Poll::Pending
    }

    /// The look after a waker clone that found a value queued.
    #[cold]
    #[inline(never)]
    fn look_again(&self) -> Poll<Option<T>> {
        if let Some(value) = self.pop_released() {
            crate::coop::spent_one();
            return Poll::Ready(Some(value));
        }
        if self.finished() {
            crate::coop::spent_one();
            return Poll::Ready(None);
        }
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
        if self.register_rx(cx.waker()) && self.len() != 0 {
            return self.look_again_many(buf, limit);
        }
        if self.finished() {
            drop(self.rx_waker.take());
            crate::coop::spent_one();
            return Poll::Ready(0);
        }
        Poll::Pending
    }

    /// As [`look_again`](Self::look_again), for a batch.
    #[cold]
    #[inline(never)]
    fn look_again_many(&self, buf: &mut Vec<T>, limit: usize) -> Poll<usize> {
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
        Poll::Pending
    }

    pub(super) fn try_recv(&self) -> Result<T, TryRecvError> {
        if let Some(value) = self.pop_released() {
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
        // A sender's waker that panics in `close` must not leave the
        // `closed()` waits asleep.
        let rest = CloseRest::new(&self.closed_waiters);
        self.limit.close();
        rest.disarm();
        wake_closed(&self.closed_waiters);
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
        // The clone is user code that may close the channel: look after it.
        let waker = cx.waker().clone();
        if this.chan.rx_closed.get() {
            drop(waker);
            return Poll::Ready(());
        }
        this.key = Some(waiters.push(0, waker));
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

/// What an unwinding [`Chan::pop_released`] does: the value goes back to the
/// front of the queue and its slot is owed again.
struct Restore<'a, T, L: Limit> {
    chan: &'a Chan<T, L>,
    value: &'a ManuallyDrop<T>,
}

impl<T, L: Limit> Drop for Restore<'_, T, L> {
    fn drop(&mut self) {
        // SAFETY: the guard is dropped only by an unwind, after which the
        // caller's `ManuallyDrop` is never read or dropped, so this is the
        // one move of the value.
        let value = unsafe { core::ptr::read(&**self.value) };
        self.chan.queue(|q| q.push_front(value));
        self.chan.limit.debit(1);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use core::cell::RefCell;
    use core::task::{Context, Poll, Waker};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::Rc;

    use crate::sync::local::mpsc::{channel, unbounded_channel};
    use crate::sync::test_support::{Polled, hooked};

    fn boom() -> Waker {
        hooked(
            None,
            Some(Box::new(|| panic!("a waker panicked as it was woken"))),
        )
    }

    #[test]
    fn a_receivers_clone_that_sends_does_not_lose_the_value() {
        let (tx, mut rx) = unbounded_channel::<u32>();
        let waker = hooked(
            Some(Box::new(move || {
                tx.send(7).unwrap();
            })),
            None,
        );
        let mut recv = Box::pin(rx.recv());
        let polled = recv.as_mut().poll(&mut Context::from_waker(&waker));
        assert_eq!(polled, Poll::Ready(Some(7)));
    }

    #[test]
    fn a_batch_receivers_clone_that_sends_does_not_lose_the_value() {
        let (tx, mut rx) = unbounded_channel::<u32>();
        let waker = hooked(
            Some(Box::new(move || {
                tx.send(7).unwrap();
            })),
            None,
        );
        let mut buf = Vec::new();
        let mut recv = Box::pin(rx.recv_many(&mut buf, 8));
        let polled = recv.as_mut().poll(&mut Context::from_waker(&waker));
        assert_eq!(polled, Poll::Ready(1));
        drop(recv);
        assert_eq!(buf, [7]);
    }

    #[test]
    fn a_receivers_clone_that_drops_the_last_sender_ends_the_stream() {
        let (tx, mut rx) = unbounded_channel::<u32>();
        let waker = hooked(Some(Box::new(move || drop(tx))), None);
        let mut recv = Box::pin(rx.recv());
        let polled = recv.as_mut().poll(&mut Context::from_waker(&waker));
        assert_eq!(polled, Poll::Ready(None));
    }

    #[test]
    fn a_closed_wait_whose_clone_closes_the_channel_completes() {
        let (tx, rx) = channel::<u32>(1);
        let rx = Rc::new(RefCell::new(rx));
        let closer = Rc::clone(&rx);
        let waker = hooked(Some(Box::new(move || closer.borrow_mut().close())), None);
        let mut closed = Box::pin(tx.closed());
        let polled = closed.as_mut().poll(&mut Context::from_waker(&waker));
        assert_eq!(polled, Poll::Ready(()));
    }

    #[test]
    fn a_refreshed_closed_wait_whose_clone_closes_the_channel_completes() {
        let (tx, rx) = channel::<u32>(1);
        let rx = Rc::new(RefCell::new(rx));
        let closer = Rc::clone(&rx);
        let mut first = Polled::new(async {});
        let mut closed = Box::pin(tx.closed());
        assert!(
            closed
                .as_mut()
                .poll(&mut Context::from_waker(&first.waker()))
                .is_pending()
        );
        let waker = hooked(Some(Box::new(move || closer.borrow_mut().close())), None);
        let polled = closed.as_mut().poll(&mut Context::from_waker(&waker));
        assert_eq!(polled, Poll::Ready(()));
        first.ready();
    }

    #[test]
    fn a_close_whose_first_waker_panics_still_wakes_the_other_closed_waits() {
        let (tx, mut rx) = channel::<u32>(1);
        let mut first = Box::pin(tx.closed());
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(&boom()))
                .is_pending()
        );
        let mut second = Polled::new(tx.closed());
        second.pending();
        let mut third = Polled::new(tx.closed());
        third.pending();
        assert!(catch_unwind(AssertUnwindSafe(|| rx.close())).is_err());
        assert_eq!(second.wakes(), 1);
        assert_eq!(third.wakes(), 1);
        second.ready();
        third.ready();
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
    }

    #[test]
    fn a_sender_waker_that_panics_does_not_lose_the_value_try_recv_popped() {
        let (tx, mut rx) = channel::<u32>(1);
        tx.try_send(1).unwrap();
        let mut send = Box::pin(tx.send(2));
        assert!(
            send.as_mut()
                .poll(&mut Context::from_waker(&boom()))
                .is_pending()
        );

        assert!(catch_unwind(AssertUnwindSafe(|| rx.try_recv())).is_err());
        assert_eq!(rx.try_recv(), Ok(1));
        let sent = send.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(sent, Poll::Ready(Ok(()))));
        assert_eq!(rx.try_recv(), Ok(2));

        assert!(tx.try_send(3).is_ok());
        assert!(tx.try_send(4).is_err(), "the capacity held at one");
    }

    #[test]
    fn a_sender_waker_that_panics_does_not_lose_the_value_recv_popped() {
        let (tx, mut rx) = channel::<u32>(1);
        tx.try_send(1).unwrap();
        let mut send = Box::pin(tx.send(2));
        assert!(
            send.as_mut()
                .poll(&mut Context::from_waker(&boom()))
                .is_pending()
        );

        let mut cx = Context::from_waker(Waker::noop());
        assert!(catch_unwind(AssertUnwindSafe(|| rx.poll_recv(&mut cx))).is_err());
        assert_eq!(rx.poll_recv(&mut cx), Poll::Ready(Some(1)));
        let sent = send.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(sent, Poll::Ready(Ok(()))));
        assert_eq!(rx.try_recv(), Ok(2));

        assert!(tx.try_send(3).is_ok());
        assert!(tx.try_send(4).is_err(), "the capacity held at one");
    }
}
