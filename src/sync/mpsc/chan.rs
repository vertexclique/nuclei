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
//!   idle exactly when no value is queued, in flight or reserved. A receive
//!   that popped a value and then unwound while giving the admission back (a
//!   sender's waker panicked) puts the value in the queue's stash, receiver
//!   owned, with its admission back: the next receive answers it first, and
//!   it counts in `len`.
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
//!   `atomic_waker` module. A push goes through the cell only when the
//!   receiver says it waits: it raises `rx_waiting` after it registered and
//!   before it looks again, and a push looks at the flag after its value is
//!   ready. Both pairs of accesses are sequentially consistent (the queue's
//!   mark, the flag), so either the receiver's look finds the value or the
//!   push finds the flag up; the push that lowers it wakes the receiver, and
//!   the pushes after it leave the cell alone. A receiver busy taking values
//!   never raises it, so a stream of sends touches nothing it writes. A value still being written (the queue answers
//!   that nothing is there yet) is one of these: its push wakes the receiver
//!   once it returned, and its sender still counts, so the receiver never
//!   takes that answer for the end.
//! - **Single consumer.** Only the [`Receiver`](super::Receiver) halves call
//!   the receive-side methods, each through `&mut self`, so there is one
//!   registrant and one taker at a time.
//!
//! # Where this file uses `unsafe`
//!
//! The queue's receiving end belongs to one thread at a time, which only the
//! receiver halves can promise (their `&mut self`), so the receive-side steps
//! here are `unsafe` functions with that contract, and nothing else is: the
//! waker cell takes care of its own slot and the counts are atomics. The auto
//! traits follow the value: a channel is `Send + Sync` when `T: Send`.

#![allow(unsafe_code)]

use core::task::{Context, Poll};
use std::panic::{RefUnwindSafe, UnwindSafe};

use super::error::TryRecvError;
use super::queue::{MessageQueue, Popped};
use crate::sync::Event;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicBool, AtomicUsize, Ordering, seq_cst_handshake};

/// A channel flavor's admission control and closed bit.
pub(super) trait Limit {
    /// The flavor's message queue.
    type Queue<T>: MessageQueue<T>;

    /// A queue for this limit.
    fn queue<T>(&self) -> Self::Queue<T>;

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
    /// A value put back by a receive a panic cut short: its admission came
    /// back already.
    Stashed(T),
    /// Empty, and every sender is gone: nothing will ever come.
    Closed,
    /// Nothing for now (or the next value is still being written: its
    /// sender wakes the receiver once its push returned).
    Empty,
}

/// A popped value whose admission is being given back: dropped by an unwind,
/// it goes to the queue's stash, where the next receive finds it. Nothing
/// here runs user code.
struct Held<'a, T, L: Limit> {
    chan: &'a Chan<T, L>,
    value: core::mem::ManuallyDrop<T>,
}

impl<T, L: Limit> Drop for Held<'_, T, L> {
    #[cold]
    fn drop(&mut self) {
        // SAFETY: dropped only by an unwind, the value still here; the
        // receive that made the guard is the channel's one receiver, and
        // runs nothing else meanwhile.
        unsafe {
            let value = core::mem::ManuallyDrop::take(&mut self.value);
            self.chan.queue.put_back(value);
        }
    }
}

/// The shared state of one channel.
pub(super) struct Chan<T, L: Limit> {
    queue: L::Queue<T>,
    /// The receiver's task, woken when a value arrives or the channel ends.
    rx_waker: AtomicWaker,
    /// The receiver registered its waker and is about to sleep: the next push
    /// lowers it and wakes the receiver (see the module).
    rx_waiting: AtomicBool,
    /// Set when the receiver closes or is dropped; `Sender::closed` waits on it.
    rx_closed: Event,
    pub(super) limit: L,
    /// How many `Sender`s (and owned permits) exist. Zero ends the channel.
    tx_count: AtomicUsize,
    /// How many `WeakSender`s exist.
    tx_weak_count: AtomicUsize,
}

// A channel holds no invariant that a panic in a caller's closure could break
// half way: every shared word is an atomic, the waker cell gives itself back
// when a waker's clone panics (see `AtomicWaker`), and a value a receive
// popped goes to the receiver's stash when a sender's waker panics, so a
// panic leaves it usable.
impl<T, L: Limit> UnwindSafe for Chan<T, L> {}
impl<T, L: Limit> RefUnwindSafe for Chan<T, L> {}

impl<T, L: Limit> Chan<T, L> {
    /// A channel with one sender counted.
    pub(super) fn new(limit: L) -> Self {
        Self {
            queue: limit.queue(),
            rx_waker: AtomicWaker::new(),
            rx_waiting: AtomicBool::new(false),
            rx_closed: Event::new(),
            limit,
            tx_count: AtomicUsize::new(1),
            tx_weak_count: AtomicUsize::new(0),
        }
    }

    // ===== the sending side =====

    /// Pushes a value the limit admitted, then wakes the receiver if it says
    /// it waits.
    ///
    /// The value is in the queue before the waker is touched, so a panic in
    /// the receiver's waker leaves the channel consistent.
    #[inline]
    pub(super) fn send(&self, value: T) {
        self.queue.push(value);
        // The push's mark is sequentially consistent, and so is this look:
        // either the receiver's look after it raised the flag finds the value,
        // or this finds the flag up (a fence for loom, whose queue model is a
        // lock).
        seq_cst_handshake();
        if self.rx_waiting.load(Ordering::SeqCst) {
            self.wake_waiting_rx();
        }
    }

    /// The wake a send owes a receiver that says it waits, out of line: a
    /// send to a receiver that is busy pays one load for it.
    #[inline(never)]
    fn wake_waiting_rx(&self) {
        if self.rx_waiting.swap(false, Ordering::SeqCst) {
            self.rx_waker.wake();
        }
    }

    /// The receiver registered its waker: it says it waits, before it looks
    /// again.
    fn rx_says_it_waits(&self) {
        self.rx_waiting.store(true, Ordering::SeqCst);
        seq_cst_handshake();
    }

    /// The receiver's look again found something: it does not wait after all.
    /// A push that saw the flag up first wakes it once more, which is harmless.
    fn rx_does_not_wait(&self) {
        self.rx_waiting.store(false, Ordering::Relaxed);
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
    ///
    /// # Safety
    ///
    /// The caller is the channel's one receiver (see the module).
    #[inline(always)]
    unsafe fn pop(&self) -> Pop<T> {
        // SAFETY: the caller's contract.
        match unsafe { self.queue.try_pop() } {
            Popped::Value(value) => return Pop::Value(value),
            Popped::Stashed(value) => return Pop::Stashed(value),
            Popped::Nothing => {}
        }
        // Count first, then look again: the count reads zero only after every
        // sender's pushes, so an empty queue behind a zero count is final. A
        // value still being written keeps its sender counted.
        if self.tx_count.load(Ordering::Acquire) != 0 {
            return Pop::Empty;
        }
        // SAFETY: the caller's contract.
        match unsafe { self.queue.try_pop() } {
            Popped::Value(value) => Pop::Value(value),
            Popped::Stashed(value) => Pop::Stashed(value),
            Popped::Nothing => Pop::Closed,
        }
    }

    /// Gives back the admission of a popped `value`. If that unwinds (the
    /// pass wakes a sender's waker), the value goes to the receiver's stash
    /// and the panic goes on: the next receive answers it.
    ///
    /// # Safety
    ///
    /// As for [`pop`](Self::pop).
    #[inline(always)]
    unsafe fn received(&self, value: T) -> T {
        let held = Held {
            chan: self,
            value: core::mem::ManuallyDrop::new(value),
        };
        self.limit.release(1);
        let mut held = core::mem::ManuallyDrop::new(held);
        // SAFETY: taken once, and the guard is never dropped after.
        unsafe { core::mem::ManuallyDrop::take(&mut held.value) }
    }

    /// One value, or the end of the channel, if either is there. A value
    /// taken here is given back to the limit.
    ///
    /// # Safety
    ///
    /// As for [`pop`](Self::pop).
    #[inline(always)]
    unsafe fn next(&self) -> Option<Option<T>> {
        // SAFETY: the caller's contract.
        match unsafe { self.pop() } {
            // SAFETY: the caller's contract.
            Pop::Value(value) => Some(Some(unsafe { self.received(value) })),
            Pop::Stashed(value) => Some(Some(value)),
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
    ///
    /// A receive that completes spends one operation of the poll's budget;
    /// with the budget spent, the poll takes nothing and yields (see `coop`).
    ///
    /// # Safety
    ///
    /// The caller is the channel's one receiver (see the module).
    pub(super) unsafe fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if crate::coop::exhausted(cx.waker()) {
            return Poll::Pending;
        }
        // SAFETY: the caller's contract.
        if let Some(done) = unsafe { self.next() } {
            crate::coop::spent_one();
            return Poll::Ready(done);
        }
        self.rx_waker.register(cx.waker());
        self.rx_says_it_waits();
        // A value pushed, or a last sender dropped, between the look above
        // and the registration is found here, or woke the waker.
        // SAFETY: the caller's contract.
        if let Some(done) = unsafe { self.next() } {
            self.rx_does_not_wait();
            crate::coop::spent_one();
            return Poll::Ready(done);
        }
        if self.finished() {
            crate::coop::spent_one();
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    /// Moves up to `limit` values into `buf`. `Some(n)` when it moved `n > 0`
    /// of them or the channel has ended (`n == 0`), `None` when it is empty
    /// for now. One release to the limit for the whole batch.
    ///
    /// # Safety
    ///
    /// As for [`pop`](Self::pop).
    unsafe fn take_many(&self, buf: &mut Vec<T>, limit: usize) -> Option<usize> {
        // `taken` values owe the limit an admission; `moved` also counts a
        // stashed one, whose admission is back.
        let mut taken = 0;
        let mut moved = 0;
        let ended = loop {
            if moved == limit {
                break false;
            }
            // SAFETY: the caller's contract.
            match unsafe { self.pop() } {
                Pop::Value(value) => {
                    buf.push(value);
                    taken += 1;
                    moved += 1;
                }
                Pop::Stashed(value) => {
                    buf.push(value);
                    moved += 1;
                }
                Pop::Closed => break true,
                Pop::Empty => break false,
            }
        };
        if taken > 0 {
            self.limit.release(taken);
        }
        (moved > 0 || ended).then_some(moved)
    }

    /// The receiver's batch poll; see [`take_many`](Self::take_many). The
    /// batch spends one operation of the poll's budget, as one receive does.
    ///
    /// # Safety
    ///
    /// The caller is the channel's one receiver (see the module).
    pub(super) unsafe fn poll_recv_many(
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
        // SAFETY: the caller's contract.
        if let Some(taken) = unsafe { self.take_many(buf, limit) } {
            crate::coop::spent_one();
            return Poll::Ready(taken);
        }
        self.rx_waker.register(cx.waker());
        self.rx_says_it_waits();
        // SAFETY: the caller's contract.
        if let Some(taken) = unsafe { self.take_many(buf, limit) } {
            self.rx_does_not_wait();
            crate::coop::spent_one();
            return Poll::Ready(taken);
        }
        if self.finished() {
            crate::coop::spent_one();
            Poll::Ready(0)
        } else {
            Poll::Pending
        }
    }

    /// The receiver's non-waiting look.
    ///
    /// It does not wait for a send that is still in progress on another
    /// thread: that value is not in the channel yet, and its sender wakes the
    /// receiver when it is.
    ///
    /// # Safety
    ///
    /// The caller is the channel's one receiver (see the module).
    pub(super) unsafe fn try_recv(&self) -> Result<T, TryRecvError> {
        // SAFETY: the caller's contract.
        match unsafe { self.pop() } {
            // SAFETY: the caller's contract.
            Pop::Value(value) => Ok(unsafe { self.received(value) }),
            Pop::Stashed(value) => Ok(value),
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
    ///
    /// It stops at a value still being written, as a receive would: that
    /// value, and any behind it, stay queued and are dropped with the channel
    /// when its last handle goes. Nothing waits for their admissions, as the
    /// channel admits nothing after the close.
    ///
    /// # Safety
    ///
    /// As for [`pop`](Self::pop).
    unsafe fn drain(&self) {
        // SAFETY: the caller's contract.
        loop {
            // SAFETY: the caller's contract.
            match unsafe { self.queue.try_pop() } {
                Popped::Value(value) => {
                    self.limit.release(1);
                    drop(value);
                }
                Popped::Stashed(value) => drop(value),
                Popped::Nothing => break,
            }
        }
    }

    /// What dropping the receiver does: close, let go of its waker so the
    /// task is not kept alive, and drop everything still queued.
    ///
    /// # Safety
    ///
    /// The caller is the channel's one receiver (see the module), dropping.
    pub(super) unsafe fn drop_rx(&self) {
        /// Finishes the drain when a value's destructor panics.
        struct Drain<'a, T, L: Limit>(&'a Chan<T, L>);

        impl<T, L: Limit> Drop for Drain<'_, T, L> {
            fn drop(&mut self) {
                // SAFETY: made only by `drop_rx`, whose caller is the receiver.
                unsafe { self.0.drain() };
            }
        }

        self.close_rx();
        drop(self.rx_waker.take());
        let guard = Drain(self);
        // SAFETY: the caller's contract.
        unsafe { guard.0.drain() };
    }
}
