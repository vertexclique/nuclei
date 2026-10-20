//! The bounded channel: [`channel`], its [`Sender`] and its [`WeakSender`].
//!
//! The channel's admission control is a [`FairSemaphore`] of `buffer`
//! permits. A send takes one permit (a reservation is exactly that), the
//! receiver gives one back per value it takes, and a permit dropped unused
//! goes back too. So the permits free, plus the ones set aside for a waiting
//! reservation, plus the values queued, plus the reservations held, are
//! always `buffer`, and the queue can never hold more than `buffer` values.
//!
//! The semaphore serves reservations strictly in the order they began to
//! wait, as the established API does: a `try_` attempt or a new reservation
//! never takes a slot while an older one waits, and a reservation for many
//! slots keeps the ones set aside for it while it waits for the rest. The
//! receiver closing closes the semaphore: waiting senders end at once, what
//! was set aside for them comes back, and every later reservation fails.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::chan::{Chan, Limit};
use super::error::{SendError, TrySendError};
use super::permit::{OwnedPermit, Permit, PermitIterator};
use super::receiver::Receiver;
use crate::sync::fair_semaphore::{FairSemaphore, FairWait};
use crate::sync::internal::{self, Arc};
use crate::sync::raw_semaphore::observed_closed;

/// The bounded flavor's limit: `bound` permits.
pub(super) struct Bounded {
    pub(super) sem: FairSemaphore,
    pub(super) bound: usize,
}

impl Limit for Bounded {
    fn is_idle(&self) -> bool {
        self.sem.available() == self.bound
    }

    fn release(&self, n: usize) {
        self.sem.release(n);
    }

    fn close(&self) {
        self.sem.close();
    }

    fn is_closed(&self) -> bool {
        self.sem.is_closed()
    }
}

/// Creates a bounded channel for communicating between tasks with
/// backpressure.
///
/// The channel buffers up to `buffer` messages. Once the buffer is full, a
/// send waits until a message is received. The buffer must hold at least one
/// message.
///
/// Everything sent on a [`Sender`] comes out of the [`Receiver`], and is meant
/// to come out in the order it was sent; under load that order can break (see
/// [Ordering](super#ordering)). The [`Sender`] can be cloned to send to the
/// same channel from many places; only one [`Receiver`] exists.
///
/// If the [`Receiver`] is closed or dropped, a send returns a
/// [`SendError`] with the value. When every [`Sender`] is dropped, `recv`
/// returns `None` once the buffered messages are received.
///
/// The message type must be `'static`: the queue behind the channel reclaims
/// its segments after the channel is gone, so it cannot hold borrowed data.
///
/// # Panics
///
/// Panics if `buffer` is 0, or larger than
/// [`Semaphore::MAX_PERMITS`](crate::sync::Semaphore::MAX_PERMITS).
///
/// ```
/// use nuclei::sync::mpsc;
///
/// let (tx, mut rx) = mpsc::channel(4);
/// let producer = std::thread::spawn(move || {
///     for i in 0..10 {
///         tx.blocking_send(i).expect("the receiver is still there");
///     }
/// });
/// for i in 0..10 {
///     assert_eq!(rx.blocking_recv(), Some(i));
/// }
/// assert_eq!(rx.blocking_recv(), None);
/// producer.join().unwrap();
/// ```
#[track_caller]
pub fn channel<T: 'static>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    assert!(buffer > 0, "mpsc bounded channel requires buffer > 0");
    let chan = Arc::new(Chan::new(Bounded {
        sem: FairSemaphore::new(buffer),
        bound: buffer,
    }));
    let tx = Sender {
        chan: Arc::clone(&chan),
    };
    (tx, Receiver::new(chan))
}

/// Sends values to the associated [`Receiver`].
///
/// Created by [`channel`]. Cloning it is a reference-count increment.
///
/// # Progress
///
/// Wait-free (a bounded number of the caller's own atomic steps whatever other
/// threads do): [`is_closed`](Self::is_closed), [`capacity`](Self::capacity),
/// [`max_capacity`](Self::max_capacity), [`same_channel`](Self::same_channel),
/// [`strong_count`](Self::strong_count), [`weak_count`](Self::weak_count),
/// [`downgrade`](Self::downgrade), `clone` and `drop`. The drop of the last
/// sender ends with a wake of the receiver's waker cell, which is two atomic
/// steps; what the waker then does belongs to the executor.
///
/// Lock-free, not wait-free: [`try_send`](Self::try_send), the `try_reserve`
/// family, and every poll of [`send`](Self::send) and the `reserve` family.
/// A permit is taken with a compare-and-swap that retries when another thread
/// changed the semaphore first; a wait that has to queue pushes a node on the
/// semaphore's lock-free arrival stack and may serve the queue (one pass, plus
/// one for each operation of another thread that lands meanwhile); and the
/// push of a value onto the message queue retries when it loses a slot to
/// another sender and allocates a segment when the last one is full. Some
/// thread's call completes whenever any retries. [`closed`](Self::closed) is
/// lock-free for the same reason.
///
/// A waiting reservation is granted by whichever thread serves the queue once
/// its slots are free (the receiver taking a value, a permit dropped), so a
/// thread the scheduler stops while it serves the queue delays that grant
/// until it runs again; for that moment a `try_` attempt fails as well, even
/// if the last waiting reservation has just left. No call waits for it.
///
/// `blocking_send` parks the calling thread until there is room.
pub struct Sender<T: 'static> {
    pub(super) chan: Arc<Chan<T, Bounded>>,
}

/// A sender that does not keep the channel open.
///
/// When every [`Sender`] is dropped and only `WeakSender`s remain, the channel
/// is closed for the receiver. A `WeakSender` sends nothing itself:
/// [`upgrade`](Self::upgrade) turns it into a [`Sender`] while one exists.
///
/// ```
/// use nuclei::sync::mpsc;
///
/// let (tx, _rx) = mpsc::channel::<i32>(15);
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
pub struct WeakSender<T: 'static> {
    chan: Arc<Chan<T, Bounded>>,
}

impl<T: 'static> Sender<T> {
    /// Sends a value, waiting until there is capacity.
    ///
    /// A successful send means the other end had not hung up when the value
    /// was published. `Ok` does not mean the value will be received: the
    /// receiver can hang up right after.
    ///
    /// # Errors
    ///
    /// If the receiving half is closed, by [`Receiver::close`] or by being
    /// dropped, the function returns an error that holds the value passed to
    /// `send`.
    ///
    /// # Cancel safety
    ///
    /// If the returned future is dropped before it completes, the message was
    /// not sent, and the value is dropped. To keep the value, reserve capacity
    /// with [`reserve`](Self::reserve) and send through the [`Permit`].
    ///
    /// Waiting sends and reservations are served in the order they began to
    /// wait. Dropping the future gives up its place, and a slot it was handed
    /// meanwhile goes on to the next one.
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
    /// let (tx, mut rx) = mpsc::channel(1);
    /// now(tx.send(1)).unwrap();
    /// assert_eq!(rx.try_recv(), Ok(1));
    ///
    /// drop(rx);
    /// let err = now(tx.send(2)).unwrap_err();
    /// assert_eq!(err.0, 2);
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
    /// let (tx, rx) = mpsc::channel::<()>(1);
    /// let mut cx = Context::from_waker(Waker::noop());
    /// let mut closed = pin!(tx.closed());
    /// assert_eq!(closed.as_mut().poll(&mut cx), Poll::Pending);
    /// drop(rx);
    /// assert_eq!(closed.as_mut().poll(&mut cx), Poll::Ready(()));
    /// ```
    pub async fn closed(&self) {
        self.chan.closed().await;
    }

    /// Sends a value at once, or says why it cannot be sent.
    ///
    /// Unlike [`send`](Self::send) it never waits: a full buffer is an error.
    /// It never takes a slot ahead of a send or reservation that is waiting.
    ///
    /// # Errors
    ///
    /// [`TrySendError::Full`] if no slot is free or a send or reservation is
    /// waiting, [`TrySendError::Closed`] if the receiver closed or was
    /// dropped. Both hold the value back.
    ///
    /// ```
    /// use nuclei::sync::mpsc::{self, error::TrySendError};
    ///
    /// let (tx, mut rx) = mpsc::channel(1);
    /// tx.try_send("first").unwrap();
    /// assert!(matches!(tx.try_send("second"), Err(TrySendError::Full("second"))));
    /// assert_eq!(rx.try_recv(), Ok("first"));
    /// ```
    pub fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        match try_reserve_inner(&self.chan, 1) {
            Ok(()) => {
                self.chan.send(message);
                Ok(())
            }
            Err(refused) => Err(refused.with_value(message)),
        }
    }

    /// Sends a value from a thread that may block.
    ///
    /// It parks the calling thread until there is room, so it must not run on
    /// a thread an executor needs for other tasks; called from inside an async
    /// task it blocks that task's thread. It works whether or not the receiver
    /// uses [`Receiver::blocking_recv`].
    ///
    /// # Panics
    ///
    /// On `wasm32`, where no other thread can make room, when the channel is
    /// full: the call could never end.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel::<u8>(1);
    /// let sender = std::thread::spawn(move || tx.blocking_send(10).unwrap());
    /// assert_eq!(rx.blocking_recv(), Some(10));
    /// sender.join().unwrap();
    /// ```
    pub fn blocking_send(&self, value: T) -> Result<(), SendError<T>> {
        internal::blocking(self.send(value))
    }

    /// Whether the channel is closed: the receiver was closed or dropped.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, rx) = mpsc::channel::<()>(42);
    /// let tx2 = tx.clone();
    /// assert!(!tx.is_closed());
    /// drop(rx);
    /// assert!(tx.is_closed() && tx2.is_closed());
    /// ```
    pub fn is_closed(&self) -> bool {
        self.chan.is_closed()
    }

    /// Waits for capacity for one message and reserves it.
    ///
    /// A [`Permit`] tracks the reserved slot; [`Permit::send`] uses it, and
    /// dropping the permit unused gives the slot back.
    ///
    /// # Errors
    ///
    /// A [`SendError`] if the receiver is closed or dropped, now or while
    /// waiting.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future gives up its place in the queue of waiters, and a
    /// slot it was handed meanwhile goes on to the next one. No slot is held
    /// by a dropped future.
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
    /// let (tx, mut rx) = mpsc::channel(1);
    /// let permit = now(tx.reserve()).unwrap();
    /// assert!(tx.try_send(123).is_err(), "the only slot is reserved");
    /// permit.send(456);
    /// assert_eq!(rx.try_recv(), Ok(456));
    /// ```
    pub async fn reserve(&self) -> Result<Permit<'_, T>, SendError<()>> {
        reserve_inner(&self.chan, 1).await?;
        Ok(Permit::new(&self.chan))
    }

    /// Waits for capacity for `n` messages and reserves them.
    ///
    /// The returned [`PermitIterator`] yields `n` permits; dropping it before
    /// it is exhausted gives the rest back. Waiting requests are served in the
    /// order they began to wait, whatever each asks for: the slots that come
    /// free are set aside for the oldest one until it has all `n`, and nothing
    /// behind it is served first. Slots set aside for a request do not count
    /// in [`capacity`](Self::capacity).
    ///
    /// # Errors
    ///
    /// A [`SendError`] if the receiver is closed or dropped, now or while
    /// waiting, or if `n` is more than the channel can ever hold
    /// ([`max_capacity`](Self::max_capacity)). `n == 0` succeeds with an empty
    /// iterator unless the channel is closed.
    ///
    /// # Cancel safety
    ///
    /// As for [`reserve`](Self::reserve): the slots set aside for a dropped
    /// request go on to the next one.
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
    /// let (tx, mut rx) = mpsc::channel(2);
    /// let mut permits = now(tx.reserve_many(2)).unwrap();
    /// assert!(tx.try_send(123).is_err());
    /// permits.next().unwrap().send(456);
    /// permits.next().unwrap().send(457);
    /// assert!(permits.next().is_none());
    /// assert_eq!(rx.try_recv(), Ok(456));
    /// assert_eq!(rx.try_recv(), Ok(457));
    /// ```
    pub async fn reserve_many(&self, n: usize) -> Result<PermitIterator<'_, T>, SendError<()>> {
        reserve_inner(&self.chan, n).await?;
        Ok(PermitIterator::new(&self.chan, n))
    }

    /// Waits for capacity for one message, moving the sender into the
    /// returned [`OwnedPermit`].
    ///
    /// Unlike [`reserve`](Self::reserve), the permit borrows nothing, so it can
    /// be held for `'static`. Cloning a sender first keeps a handle to send
    /// with meanwhile. [`OwnedPermit::send`] and [`OwnedPermit::release`] hand
    /// the sender back.
    ///
    /// # Errors
    ///
    /// A [`SendError`] if the receiver is closed or dropped, now or while
    /// waiting.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future gives up its place in the queue of waiters, passes
    /// on a slot it was handed meanwhile, and drops the sender it holds.
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
    /// let (tx, mut rx) = mpsc::channel(1);
    /// let permit = now(tx.reserve_owned()).unwrap();
    /// let tx = permit.send(123);
    /// assert_eq!(rx.try_recv(), Ok(123));
    /// tx.try_send(456).unwrap();
    /// ```
    pub async fn reserve_owned(self) -> Result<OwnedPermit<T>, SendError<()>> {
        reserve_inner(&self.chan, 1).await?;
        Ok(OwnedPermit::new(self))
    }

    /// Reserves capacity for one message if there is any, without waiting.
    /// It never takes a slot ahead of a send or reservation that is waiting.
    ///
    /// # Errors
    ///
    /// [`TrySendError::Full`] when no slot is free or a send or reservation
    /// is waiting, [`TrySendError::Closed`] when the receiver closed or was
    /// dropped.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel(1);
    /// let permit = tx.try_reserve().unwrap();
    /// assert!(tx.try_reserve().is_err(), "no slot is left");
    /// permit.send("foo");
    /// assert_eq!(rx.try_recv(), Ok("foo"));
    /// ```
    pub fn try_reserve(&self) -> Result<Permit<'_, T>, TrySendError<()>> {
        try_reserve_inner(&self.chan, 1)?;
        Ok(Permit::new(&self.chan))
    }

    /// Reserves capacity for `n` messages if there is enough, without
    /// waiting.
    ///
    /// # Errors
    ///
    /// [`TrySendError::Full`] when fewer than `n` slots are free, a send or
    /// reservation is waiting, or `n` is more than the channel can ever hold;
    /// [`TrySendError::Closed`] when the receiver closed or was dropped.
    /// `n == 0` succeeds with an empty iterator unless the channel is closed.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel(2);
    /// let mut permits = tx.try_reserve_many(2).unwrap();
    /// assert!(tx.try_send(123).is_err());
    /// permits.next().unwrap().send(456);
    /// permits.next().unwrap().send(457);
    /// assert_eq!(rx.try_recv(), Ok(456));
    /// assert_eq!(rx.try_recv(), Ok(457));
    ///
    /// assert!(tx.try_reserve_many(3).is_err(), "more than the channel holds");
    /// ```
    pub fn try_reserve_many(&self, n: usize) -> Result<PermitIterator<'_, T>, TrySendError<()>> {
        if n > self.chan.limit.bound {
            return Err(TrySendError::Full(()));
        }
        try_reserve_inner(&self.chan, n)?;
        Ok(PermitIterator::new(&self.chan, n))
    }

    /// Reserves capacity for one message if there is any, moving the sender
    /// into the returned [`OwnedPermit`].
    ///
    /// # Errors
    ///
    /// On failure the sender is handed back inside the error:
    /// [`TrySendError::Full`] when no slot is free or a send or reservation
    /// is waiting, [`TrySendError::Closed`] when the receiver closed or was
    /// dropped.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel(1);
    /// let permit = tx.clone().try_reserve_owned().unwrap();
    /// assert!(tx.try_send(123).is_err());
    /// permit.send(456);
    /// assert_eq!(rx.try_recv(), Ok(456));
    /// ```
    pub fn try_reserve_owned(self) -> Result<OwnedPermit<T>, TrySendError<Self>> {
        match try_reserve_inner(&self.chan, 1) {
            Ok(()) => Ok(OwnedPermit::new(self)),
            Err(refused) => Err(refused.with_value(self)),
        }
    }

    /// Whether two senders belong to the same channel.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, _rx) = mpsc::channel::<()>(1);
    /// let tx2 = tx.clone();
    /// assert!(tx.same_channel(&tx2));
    ///
    /// let (tx3, _rx3) = mpsc::channel::<()>(1);
    /// assert!(!tx3.same_channel(&tx2));
    /// ```
    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.chan, &other.chan)
    }

    /// The channel's current capacity: the slots that are free.
    ///
    /// It goes down when a value is sent or a slot is reserved, and up when the
    /// receiver takes a value or a permit is dropped unused. A slot set aside
    /// for a waiting [`reserve_many`](Self::reserve_many) is not free. Compare
    /// [`max_capacity`](Self::max_capacity), which never changes.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel::<()>(5);
    /// assert_eq!(tx.capacity(), 5);
    /// let permit = tx.try_reserve().unwrap();
    /// assert_eq!(tx.capacity(), 4);
    /// permit.send(());
    /// rx.try_recv().unwrap();
    /// assert_eq!(tx.capacity(), 5);
    /// ```
    pub fn capacity(&self) -> usize {
        self.chan.limit.sem.available()
    }

    /// Converts the sender into a [`WeakSender`], which does not keep the
    /// channel open. The sender itself stays.
    #[must_use = "Downgrade creates a WeakSender without destroying the original non-weak sender."]
    pub fn downgrade(&self) -> WeakSender<T> {
        self.chan.add_weak();
        WeakSender {
            chan: Arc::clone(&self.chan),
        }
    }

    /// The channel's maximum capacity: the `buffer` it was created with.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, _rx) = mpsc::channel::<()>(5);
    /// let _permit = tx.try_reserve().unwrap();
    /// assert_eq!(tx.max_capacity(), 5);
    /// assert_eq!(tx.capacity(), 4);
    /// ```
    pub fn max_capacity(&self) -> usize {
        self.chan.limit.bound
    }

    /// How many [`Sender`] handles exist (an [`OwnedPermit`] holds one).
    pub fn strong_count(&self) -> usize {
        self.chan.strong_count()
    }

    /// How many [`WeakSender`] handles exist.
    pub fn weak_count(&self) -> usize {
        self.chan.weak_count()
    }
}

impl<T: 'static> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.chan.add_sender();
        Sender {
            chan: Arc::clone(&self.chan),
        }
    }
}

impl<T: 'static> Drop for Sender<T> {
    fn drop(&mut self) {
        self.chan.drop_sender();
    }
}

impl<T: 'static> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("closed", &self.is_closed())
            .field("capacity", &self.capacity())
            .field("max_capacity", &self.max_capacity())
            .finish()
    }
}

impl<T: 'static> WeakSender<T> {
    /// Tries to turn the weak sender into a [`Sender`].
    ///
    /// Returns `Some` while at least one [`Sender`] (or [`OwnedPermit`]) is
    /// alive, and `None` once the last is gone.
    pub fn upgrade(&self) -> Option<Sender<T>> {
        self.chan.try_add_sender().then(|| Sender {
            chan: Arc::clone(&self.chan),
        })
    }

    /// How many [`Sender`] handles exist.
    pub fn strong_count(&self) -> usize {
        self.chan.strong_count()
    }

    /// How many [`WeakSender`] handles exist.
    pub fn weak_count(&self) -> usize {
        self.chan.weak_count()
    }
}

impl<T: 'static> Clone for WeakSender<T> {
    fn clone(&self) -> Self {
        self.chan.add_weak();
        WeakSender {
            chan: Arc::clone(&self.chan),
        }
    }
}

impl<T: 'static> Drop for WeakSender<T> {
    fn drop(&mut self) {
        self.chan.drop_weak();
    }
}

impl<T: 'static> fmt::Debug for WeakSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WeakSender").finish()
    }
}

/// Takes `n` slots without waiting: the permits are the caller's to hand to
/// [`Permit`]s, which return them.
fn try_reserve_inner<T: 'static>(
    chan: &Chan<T, Bounded>,
    n: usize,
) -> Result<(), TrySendError<()>> {
    match chan.limit.sem.try_acquire(n) {
        Ok(()) => Ok(()),
        Err(seen) if observed_closed(seen) => Err(TrySendError::Closed(())),
        Err(_) => Err(TrySendError::Full(())),
    }
}

/// Waits for `n` slots, which the caller then owns as [`Permit`]s.
async fn reserve_inner<T: 'static>(chan: &Chan<T, Bounded>, n: usize) -> Result<(), SendError<()>> {
    if n > chan.limit.bound {
        return Err(SendError(()));
    }
    Reserve {
        chan,
        n,
        wait: FairWait::new(),
        taken: false,
    }
    .await
}

/// The wait of [`reserve_inner`].
///
/// A wait that ends without its slots becoming the caller's (it failed, or
/// it was dropped, perhaps just after a grant) can leave a closed channel
/// idle once what it held is back, with no receiver call to notice: so its
/// drop wakes a receiver that waits for a closed channel to go idle, after
/// the wait has left the queue.
struct Reserve<'a, T: 'static> {
    chan: &'a Chan<T, Bounded>,
    n: usize,
    wait: FairWait,
    /// The slots are the caller's: they leave with its permits.
    taken: bool,
}

impl<T: 'static> Future for Reserve<'_, T> {
    type Output = Result<(), SendError<()>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.wait
            .poll(&this.chan.limit.sem, this.n, cx.waker())
            .map(|taken| {
                this.taken = taken.is_ok();
                taken.map_err(|_| SendError(()))
            })
    }
}

impl<T: 'static> Drop for Reserve<'_, T> {
    fn drop(&mut self) {
        if !self.taken {
            self.wait.cancel(&self.chan.limit.sem, self.n);
            self.chan.wake_if_finished();
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::Polled;

    #[test]
    fn the_free_slots_plus_the_queued_values_are_the_buffer() {
        let (tx, mut rx) = channel::<u32>(4);
        let permit = tx.try_reserve().unwrap();
        tx.try_send(1).unwrap();
        assert_eq!(tx.capacity() + rx.len() + 1, 4);
        permit.send(2);
        assert_eq!(tx.capacity() + rx.len(), 4);
        rx.try_recv().unwrap();
        assert_eq!(tx.capacity() + rx.len(), 4);
    }

    #[test]
    fn a_zero_buffer_is_refused() {
        let refused = std::panic::catch_unwind(|| channel::<u8>(0));
        assert!(refused.is_err());
    }

    #[test]
    fn a_reserve_granted_then_dropped_after_the_close_wakes_the_receiver() {
        let (tx, mut rx) = channel::<()>(1);
        let held = tx.try_reserve().unwrap();
        let mut reserve = Polled::new(tx.reserve());
        reserve.pending();
        drop(held);
        assert_eq!(reserve.wakes(), 1, "the slot was granted to the waiter");
        assert_eq!(tx.capacity(), 0);
        rx.close();
        let mut recv = Polled::new(rx.recv());
        recv.pending();
        drop(reserve);
        assert_eq!(tx.capacity(), 1, "the granted slot came back");
        assert_eq!(recv.wakes(), 1, "and the closed receiver was told");
        assert_eq!(recv.ready(), None);
    }

    #[test]
    fn a_failed_reserve_wakes_a_receiver_waiting_for_idle() {
        let (tx, mut rx) = channel::<()>(1);
        let permit = tx.try_reserve().unwrap();
        let mut reserve = Polled::new(tx.reserve());
        reserve.pending();
        rx.close();
        // The reservation was granted the error by the close.
        assert!(reserve.wakes() >= 1);
        assert!(matches!(reserve.ready(), Err(SendError(()))));
        let mut recv = Polled::new(rx.recv());
        recv.pending();
        drop(permit);
        assert_eq!(recv.wakes(), 1, "the last permit back ends the wait");
        assert_eq!(recv.ready(), None);
    }

    #[test]
    fn dropping_the_receiver_fails_a_waiting_reserve_with_the_value_kept() {
        let (tx, rx) = channel::<String>(1);
        tx.try_send("held".to_string()).unwrap();
        let mut send = Polled::new(tx.send("kept".to_string()));
        send.pending();
        drop(rx);
        let SendError(value) = send.ready().unwrap_err();
        assert_eq!(value, "kept");
    }
}
