//! A multi-producer, multi-consumer broadcast channel: each value sent
//! reaches every receiver that exists when it is sent.
//!
//! [`channel`] returns a [`Sender`] and a [`Receiver`] around a capacity.
//! [`Sender::send`] puts a value in the channel for every receiver; each
//! [`Receiver`] takes the values in order with [`Receiver::recv`], cloning
//! them (`T: Clone`). The channel holds the last `capacity` values (rounded
//! up to a power of two): a receiver that falls further behind loses the
//! oldest ones, and its next receive reports how many with
//! [`RecvError::Lagged`] before it goes on from the oldest value still
//! held. The channel closes once every sender is gone: receivers take what
//! is left, then get [`RecvError::Closed`]. The API is the established
//! async broadcast-channel API; nothing here needs a runtime.
//!
//! ```
//! use nuclei::sync::broadcast;
//!
//! let (tx, mut rx1) = broadcast::channel(16);
//! let mut rx2 = tx.subscribe();
//! tx.send(10).unwrap();
//! tx.send(20).unwrap();
//! assert_eq!(rx1.try_recv(), Ok(10));
//! assert_eq!(rx2.try_recv(), Ok(10));
//! assert_eq!(rx1.try_recv(), Ok(20));
//! ```
//!
//! # Values live in nodes published lock-free
//!
//! The channel is a ring of `capacity` slots, each an atomic pointer
//! holding the node of the latest value sent to it with that value's
//! position. A sender takes the next position with one `fetch_add`,
//! publishes its node into the position's slot with a compare-and-swap that
//! never replaces a later position, and retires the node it replaced. A
//! receive is a protected load of its slot and a clone of the value: it
//! writes nothing another thread reads, so receivers never contend with
//! one another or with the senders. Two consequences, both differing from
//! the lock-based API:
//!
//! - The channel needs `T: Send + Sync + 'static`: values are shared by
//!   reference between threads and reclaimed later.
//! - A value is dropped when a later send takes its slot (or the channel
//!   goes), not as soon as every receiver has taken it: the channel holds
//!   up to `capacity` values whatever the receivers have read. [`Sender::len`]
//!   still counts what the slowest receiver has yet to take.
//!
//! Under the `recycle` feature its nodes come from a per-thread cache instead
//! of the global allocator.
//!
//! # Progress
//!
//! - Wait-free: [`Receiver::try_recv`], [`Receiver::len`],
//!   [`Receiver::is_empty`], [`Sender::receiver_count`],
//!   [`Sender::same_channel`], [`Sender::subscribe`],
//!   [`Receiver::resubscribe`], [`Sender::downgrade`], the sender counts,
//!   and the clones and drops but the last sender's and the last
//!   receiver's.
//! - Lock-free: [`Sender::send`] (its compare-and-swap is retried only if a
//!   send a whole ring later published into the same slot meanwhile, and it
//!   ends by waking the waiting receivers through
//!   [`Notify::notify_waiters`](super::Notify), which is lock-free);
//!   [`Sender::len`] and [`Sender::is_empty`] (they look at every
//!   receiver); polling [`Receiver::recv`] and [`Sender::closed`] (a poll
//!   may queue a waiter); [`WeakSender::upgrade`] (a compare-and-swap on
//!   the sender count, retried when a sender was cloned or dropped
//!   meanwhile); and the last sender's or receiver's drop, for its wake.

#![allow(unsafe_code)]

use core::fmt;

use kovan::{
    Atomic, CachePadded, RetiredNode, Shared as Ptr, alloc_node, drop_node, pin, retire_node,
};

use super::Notify;
use super::internal::{Arc, AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub use self::error::{RecvError, SendError, TryRecvError};

pub mod error {
    //! Error types of the broadcast channel.

    use core::fmt;
    use std::error::Error;

    /// Error produced when sending a value fails: there is no receiver.
    ///
    /// The value is handed back in the error.
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

    /// Error produced when a receive fails.
    #[derive(Debug, PartialEq, Eq, Clone)]
    pub enum RecvError {
        /// Every sender is gone and every value was taken.
        Closed,
        /// The receiver fell behind by this many values, which are lost;
        /// the next receive takes the oldest value the channel still holds.
        Lagged(u64),
    }

    impl fmt::Display for RecvError {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                RecvError::Closed => write!(fmt, "channel closed"),
                RecvError::Lagged(missed) => write!(fmt, "channel lagged by {missed}"),
            }
        }
    }

    impl Error for RecvError {}

    /// Error produced when a receive that does not wait fails.
    #[derive(Debug, PartialEq, Eq, Clone)]
    pub enum TryRecvError {
        /// No value has come since the last one this receiver took.
        Empty,
        /// Every sender is gone and every value was taken.
        Closed,
        /// The receiver fell behind by this many values, which are lost;
        /// the next receive takes the oldest value the channel still holds.
        Lagged(u64),
    }

    impl fmt::Display for TryRecvError {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                TryRecvError::Empty => write!(fmt, "channel empty"),
                TryRecvError::Closed => write!(fmt, "channel closed"),
                TryRecvError::Lagged(missed) => write!(fmt, "channel lagged by {missed}"),
            }
        }
    }

    impl Error for TryRecvError {}
}

/// A value in the ring, with its position.
#[repr(C)]
struct Node<T> {
    retired: RetiredNode,
    pos: u64,
    value: T,
}

/// Where a receiver is: the position of the next value it takes. Only its
/// receiver writes it; [`Sender::len`] reads every one.
struct Cursor {
    next: AtomicU64,
}

struct Shared<T: Send + Sync + 'static> {
    /// The slots, each the node of the latest position sent to it.
    slots: Box<[Atomic<Node<T>>]>,
    /// `slots.len() - 1`: a position's slot is `pos & mask`.
    mask: u64,
    /// The next position a send takes. Padded: every send writes it.
    tail: CachePadded<AtomicU64>,
    /// Every sender is gone.
    closed: AtomicBool,
    /// How many [`Sender`]s exist. Once zero it stays zero: a sender is
    /// made only from another, or by an upgrade that finds one.
    senders: AtomicUsize,
    /// How many [`WeakSender`]s exist.
    weak_senders: AtomicUsize,
    /// The receivers' cursors, by id; their count is the receiver count.
    cursors: kovan_map::HashMap<u64, Arc<Cursor>>,
    receivers: AtomicUsize,
    /// Wakes receivers waiting for a value or the close.
    notify_rx: Notify,
    /// Wakes senders waiting for the receivers to go.
    notify_tx: Notify,
}

// SAFETY: the slots' nodes hold `T`s that are moved between threads and
// read by reference from several at once: `T: Send + Sync` covers both.
unsafe impl<T: Send + Sync + 'static> Send for Shared<T> {}
// SAFETY: as above.
unsafe impl<T: Send + Sync + 'static> Sync for Shared<T> {}

impl<T: Send + Sync + 'static> Shared<T> {
    fn capacity(&self) -> u64 {
        self.mask + 1
    }

    /// A new receiver, which takes the values sent from now on.
    fn subscribe(shared: &Arc<Self>) -> Receiver<T> {
        // The ids only tell receivers apart, so the counter is std's even
        // under loom, which has no const constructor for a static.
        static IDS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let id = IDS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // SeqCst: against a send's claim of its position (also SeqCst), so
        // the value of a position at or past this one reaches the receiver.
        let cursor = Arc::new(Cursor {
            next: AtomicU64::new(shared.tail.load(Ordering::SeqCst)),
        });
        shared.receivers.fetch_add(1, Ordering::SeqCst);
        shared.cursors.insert(id, Arc::clone(&cursor));
        Receiver {
            shared: Arc::clone(shared),
            id,
            cursor,
        }
    }
}

impl<T: Send + Sync + 'static> Drop for Shared<T> {
    fn drop(&mut self) {
        let guard = pin();
        for slot in self.slots.iter() {
            let node = slot.load(Ordering::Acquire, &guard);
            if !node.is_null() {
                // SAFETY: no handle is left, so no thread can reach the slot,
                // and the node came from `alloc_node`; let go once, here.
                unsafe { drop_node(node.as_raw()) };
            }
        }
    }
}

/// Creates a broadcast channel holding the last `capacity` values (rounded
/// up to a power of two), returning a sender and a first receiver.
///
/// # Panics
///
/// When `capacity` is zero, or above `usize::MAX / 2`.
///
/// ```
/// use nuclei::sync::broadcast;
///
/// let (tx, mut rx) = broadcast::channel::<u32>(4);
/// tx.send(1).unwrap();
/// assert_eq!(rx.try_recv(), Ok(1));
/// ```
pub fn channel<T: Clone + Send + Sync + 'static>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let tx = Sender::new(capacity);
    let rx = tx.subscribe();
    (tx, rx)
}

/// Sends values to every [`Receiver`] of the channel.
///
/// Created by [`channel`]. Clone it for more producers. The channel closes
/// for the receivers when the last sender is dropped.
pub struct Sender<T: Send + Sync + 'static> {
    shared: Arc<Shared<T>>,
}

/// Receives the values of the channel, in order.
///
/// Created by [`channel`], [`Sender::subscribe`] or
/// [`Receiver::resubscribe`]; each takes every value sent after it was made.
pub struct Receiver<T: Send + Sync + 'static> {
    shared: Arc<Shared<T>>,
    id: u64,
    cursor: Arc<Cursor>,
}

/// A sender that does not keep the channel open.
///
/// Made by [`Sender::downgrade`]. Once every [`Sender`] is gone the channel
/// is closed, whatever `WeakSender`s are left. A `WeakSender` sends nothing
/// itself: [`upgrade`](Self::upgrade) makes a [`Sender`] of it while one
/// exists.
///
/// ```
/// use nuclei::sync::broadcast;
///
/// let (tx, _rx) = broadcast::channel::<u8>(4);
/// let weak = tx.downgrade();
/// assert!(weak.upgrade().is_some());
/// drop(tx);
/// assert!(weak.upgrade().is_none());
/// ```
pub struct WeakSender<T: Send + Sync + 'static> {
    shared: Arc<Shared<T>>,
}

impl<T: Send + Sync + 'static> Sender<T> {
    /// A sender of a new channel holding the last `capacity` values (rounded
    /// up to a power of two), with no receiver yet: each comes from
    /// [`subscribe`](Self::subscribe).
    ///
    /// # Panics
    ///
    /// When `capacity` is zero, or above `usize::MAX / 2`.
    ///
    /// ```
    /// use nuclei::sync::broadcast::Sender;
    ///
    /// let tx = Sender::new(4);
    /// assert!(tx.send(1).is_err(), "no receiver yet");
    /// let mut rx = tx.subscribe();
    /// tx.send(2).unwrap();
    /// assert_eq!(rx.try_recv(), Ok(2));
    /// ```
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity > 0,
            "a broadcast channel's capacity cannot be zero"
        );
        assert!(
            capacity <= usize::MAX >> 1,
            "a broadcast channel's capacity cannot exceed usize::MAX / 2"
        );
        let capacity = capacity.next_power_of_two();
        Self {
            shared: Arc::new(Shared {
                slots: (0..capacity).map(|_| Atomic::null()).collect(),
                mask: capacity as u64 - 1,
                tail: CachePadded::new(AtomicU64::new(0)),
                closed: AtomicBool::new(false),
                senders: AtomicUsize::new(1),
                weak_senders: AtomicUsize::new(0),
                // 64 is the smallest table the map accepts.
                cursors: kovan_map::HashMap::with_capacity(64),
                receivers: AtomicUsize::new(0),
                notify_rx: Notify::new(),
                notify_tx: Notify::new(),
            }),
        }
    }

    /// Sends `value` to every receiver there is now, returning how many.
    ///
    /// A send never waits: the channel holds the last `capacity` values, and
    /// a receiver too far behind loses the oldest.
    ///
    /// # Errors
    ///
    /// When there is no receiver; the value is handed back.
    ///
    /// ```
    /// use nuclei::sync::broadcast;
    ///
    /// let (tx, rx) = broadcast::channel(2);
    /// assert_eq!(tx.send("a"), Ok(1));
    /// drop(rx);
    /// assert_eq!(tx.send("b").unwrap_err().0, "b");
    /// ```
    pub fn send(&self, value: T) -> Result<usize, SendError<T>> {
        let shared = &*self.shared;
        let receivers = shared.receivers.load(Ordering::SeqCst);
        if receivers == 0 {
            return Err(SendError(value));
        }
        let pos = shared.tail.fetch_add(1, Ordering::SeqCst);
        let slot = &shared.slots[(pos & shared.mask) as usize];
        let node = alloc_node(Node {
            retired: RetiredNode::new(),
            pos,
            value,
        });
        let guard = pin();
        let mut current = slot.load(Ordering::Acquire, &guard);
        loop {
            // SAFETY: the guard keeps the slot's node alive while it is read.
            if unsafe { current.as_ref() }.is_some_and(|old| old.pos > pos) {
                // A send a whole ring later took the slot first: every
                // receiver is past this value already, which goes now.
                // SAFETY: never published; ours alone.
                unsafe { drop_node(node) };
                break;
            }
            // SAFETY: a fresh node, published here once.
            let new = unsafe { Ptr::from_raw(node) };
            match slot.compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire, &guard) {
                Ok(old) => {
                    if !old.is_null() {
                        // SAFETY: the swap unlinked `old`; retired once, here.
                        unsafe { retire_node(old.as_raw()) };
                    }
                    break;
                }
                Err(now) => current = now,
            }
        }
        drop(guard);
        shared.notify_rx.notify_waiters();
        Ok(receivers)
    }

    /// A new receiver, which takes the values sent from now on.
    ///
    /// ```
    /// use nuclei::sync::broadcast;
    ///
    /// let (tx, _rx) = broadcast::channel(2);
    /// tx.send(1).unwrap();
    /// let mut late = tx.subscribe();
    /// tx.send(2).unwrap();
    /// assert_eq!(late.try_recv(), Ok(2));
    /// ```
    pub fn subscribe(&self) -> Receiver<T> {
        Shared::subscribe(&self.shared)
    }

    /// How many receivers there are.
    pub fn receiver_count(&self) -> usize {
        self.shared.receivers.load(Ordering::SeqCst)
    }

    /// A [`WeakSender`] of the channel, which does not keep it open. The
    /// sender itself stays.
    #[must_use = "a WeakSender that is not kept does nothing"]
    pub fn downgrade(&self) -> WeakSender<T> {
        // Relaxed: a plain counter.
        self.shared.weak_senders.fetch_add(1, Ordering::Relaxed);
        WeakSender {
            shared: Arc::clone(&self.shared),
        }
    }

    /// How many [`Sender`]s exist.
    pub fn strong_count(&self) -> usize {
        self.shared.senders.load(Ordering::Acquire)
    }

    /// How many [`WeakSender`]s exist.
    pub fn weak_count(&self) -> usize {
        self.shared.weak_senders.load(Ordering::Relaxed)
    }

    /// How many values the slowest receiver has yet to take, at most the
    /// capacity (older ones are lost to it); zero with no receiver.
    ///
    /// ```
    /// use nuclei::sync::broadcast;
    ///
    /// let (tx, mut rx) = broadcast::channel(4);
    /// tx.send(1).unwrap();
    /// tx.send(2).unwrap();
    /// assert_eq!(tx.len(), 2);
    /// rx.try_recv().unwrap();
    /// assert_eq!(tx.len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        let shared = &*self.shared;
        let tail = shared.tail.load(Ordering::SeqCst);
        let behind = shared
            .cursors
            .iter()
            .map(|(_, cursor)| tail.saturating_sub(cursor.next.load(Ordering::Acquire)))
            .max()
            .unwrap_or(0);
        behind.min(shared.capacity()) as usize
    }

    /// Whether every receiver took every value it can still take.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `other` sends on the same channel.
    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// Completes once there is no receiver.
    ///
    /// ```
    /// use core::future::Future;
    /// use core::pin::pin;
    /// use core::task::{Context, Poll, Waker};
    /// use nuclei::sync::broadcast;
    ///
    /// let (tx, rx) = broadcast::channel::<u8>(1);
    /// let mut cx = Context::from_waker(Waker::noop());
    /// let mut closed = pin!(tx.closed());
    /// assert_eq!(closed.as_mut().poll(&mut cx), Poll::Pending);
    /// drop(rx);
    /// assert_eq!(closed.as_mut().poll(&mut cx), Poll::Ready(()));
    /// ```
    pub async fn closed(&self) {
        while self.receiver_count() > 0 {
            // Made before the count is read again: the last receiver's drop
            // wakes it after its decrement.
            let notified = self.shared.notify_tx.notified();
            if self.receiver_count() == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl<T: Send + Sync + 'static> Clone for Sender<T> {
    fn clone(&self) -> Self {
        // Relaxed: a plain counter, and the clone proves a sender exists.
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T: Send + Sync + 'static> Drop for Sender<T> {
    fn drop(&mut self) {
        // AcqRel: the thread that drops the last sender sees every earlier
        // sender's sends before it closes the channel.
        if self.shared.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.closed.store(true, Ordering::SeqCst);
            self.shared.notify_rx.notify_waiters();
        }
    }
}

impl<T: Send + Sync + 'static> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender")
            .field("receivers", &self.receiver_count())
            .finish_non_exhaustive()
    }
}

impl<T: Send + Sync + 'static> WeakSender<T> {
    /// A [`Sender`] of the channel, while one exists; `None` once the last
    /// is gone (the channel is closed then, for good).
    pub fn upgrade(&self) -> Option<Sender<T>> {
        let senders = &self.shared.senders;
        // Acquire on a success, as a clone's: the new sender's sends come
        // after the close only if no sender was left, which this rules out.
        let mut count = senders.load(Ordering::Acquire);
        while count > 0 {
            match senders.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Sender {
                        shared: Arc::clone(&self.shared),
                    });
                }
                Err(now) => count = now,
            }
        }
        None
    }

    /// How many [`Sender`]s exist.
    pub fn strong_count(&self) -> usize {
        self.shared.senders.load(Ordering::Acquire)
    }

    /// How many [`WeakSender`]s exist.
    pub fn weak_count(&self) -> usize {
        self.shared.weak_senders.load(Ordering::Relaxed)
    }
}

impl<T: Send + Sync + 'static> Clone for WeakSender<T> {
    fn clone(&self) -> Self {
        self.shared.weak_senders.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T: Send + Sync + 'static> Drop for WeakSender<T> {
    fn drop(&mut self) {
        self.shared.weak_senders.fetch_sub(1, Ordering::Relaxed);
    }
}

impl<T: Send + Sync + 'static> fmt::Debug for WeakSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WeakSender").finish_non_exhaustive()
    }
}

impl<T: Send + Sync + 'static> Receiver<T> {
    fn next(&self) -> u64 {
        // Relaxed: only this receiver writes it.
        self.cursor.next.load(Ordering::Relaxed)
    }

    fn advance(&self, next: u64) {
        // Release: `Sender::len` reads where the receiver is.
        self.cursor.next.store(next, Ordering::Release);
    }

    /// How many values were sent that this receiver has yet to take; past
    /// the capacity, its next receive reports a lag.
    pub fn len(&self) -> usize {
        let tail = self.shared.tail.load(Ordering::SeqCst);
        tail.saturating_sub(self.next()) as usize
    }

    /// Whether this receiver took every value sent.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A new receiver, which takes the values sent from now on.
    pub fn resubscribe(&self) -> Self {
        Shared::subscribe(&self.shared)
    }
}

impl<T: Clone + Send + Sync + 'static> Receiver<T> {
    /// Takes the next value, if one came, without waiting.
    ///
    /// # Errors
    ///
    /// [`TryRecvError::Empty`] when none came since the last one taken;
    /// [`TryRecvError::Lagged`] once, when the receiver lost values, before
    /// it goes on from the oldest one still held; [`TryRecvError::Closed`]
    /// once every sender is gone and every value was taken.
    ///
    /// ```
    /// use nuclei::sync::broadcast::{self, error::TryRecvError};
    ///
    /// let (tx, mut rx) = broadcast::channel(2);
    /// for i in 0..3 {
    ///     tx.send(i).unwrap();
    /// }
    /// assert_eq!(rx.try_recv(), Err(TryRecvError::Lagged(1)));
    /// assert_eq!(rx.try_recv(), Ok(1));
    /// assert_eq!(rx.try_recv(), Ok(2));
    /// assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    /// drop(tx);
    /// assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    /// ```
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let shared = &*self.shared;
        let next = self.next();
        let guard = pin();
        let slot = &shared.slots[(next & shared.mask) as usize];
        // SAFETY: the guard keeps the node alive while it is read.
        match unsafe { slot.load(Ordering::Acquire, &guard).as_ref() } {
            Some(node) if node.pos == next => {
                let value = node.value.clone();
                drop(guard);
                self.advance(next + 1);
                return Ok(value);
            }
            Some(node) if node.pos > next => {
                drop(guard);
                // A later position took the slot: positions before the oldest
                // the ring can hold are lost. That one is past `next`, since
                // the slot's position is at least a ring past it and below
                // the tail.
                let oldest = shared
                    .tail
                    .load(Ordering::SeqCst)
                    .saturating_sub(shared.capacity());
                self.advance(oldest);
                return Err(TryRecvError::Lagged(oldest - next));
            }
            // Not sent yet, or sent and not published yet.
            _ => {}
        }
        drop(guard);
        // SeqCst, the flag before the tail: the last sender's close comes
        // after every send, so with the flag seen every position below the
        // tail is published, and one at `next` would have been found.
        if shared.closed.load(Ordering::SeqCst) && shared.tail.load(Ordering::SeqCst) <= next {
            return Err(TryRecvError::Closed);
        }
        Err(TryRecvError::Empty)
    }

    /// Waits for the next value and takes it.
    ///
    /// # Errors
    ///
    /// [`RecvError::Lagged`] once, when the receiver lost values, before it
    /// goes on from the oldest one still held; [`RecvError::Closed`] once
    /// every sender is gone and every value was taken.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future takes nothing: the next receive gets the value.
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        let shared = Arc::clone(&self.shared);
        loop {
            // Made before the look: a send after the look wakes it (see
            // `Notify`: a `Notified` hears every `notify_waiters` made after
            // its creation).
            let notified = shared.notify_rx.notified();
            match self.try_recv() {
                Ok(value) => return Ok(value),
                Err(TryRecvError::Lagged(missed)) => return Err(RecvError::Lagged(missed)),
                Err(TryRecvError::Closed) => return Err(RecvError::Closed),
                Err(TryRecvError::Empty) => notified.await,
            }
        }
    }

    /// Receives the next value from a thread that may block.
    ///
    /// Like [`recv`](Self::recv), but it parks the calling thread while no
    /// value came and the channel is open. On a nuclei shard (inside one of
    /// its tasks) it does not park the thread: it runs a nested `block_on`,
    /// so the shard keeps running its other tasks meanwhile. Inside another
    /// runtime's task it blocks that task's thread.
    ///
    /// # Errors
    ///
    /// As for [`recv`](Self::recv).
    ///
    /// # Panics
    ///
    /// On `wasm32`, where no other thread can send, when no value came and
    /// the channel is open: the call could never end.
    ///
    /// ```
    /// use nuclei::sync::broadcast::{self, RecvError};
    ///
    /// let (tx, mut rx) = broadcast::channel::<u8>(4);
    /// let receiver = std::thread::spawn(move || {
    ///     assert_eq!(rx.blocking_recv(), Ok(10));
    ///     assert_eq!(rx.blocking_recv(), Err(RecvError::Closed));
    /// });
    /// tx.send(10).unwrap();
    /// drop(tx);
    /// receiver.join().unwrap();
    /// ```
    pub fn blocking_recv(&mut self) -> Result<T, RecvError> {
        super::internal::blocking(self.recv())
    }
}

impl<T: Send + Sync + 'static> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.cursors.remove(&self.id);
        if self.shared.receivers.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.shared.notify_tx.notify_waiters();
        }
    }
}

impl<T: Send + Sync + 'static> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receiver")
            .field("next", &self.next())
            .finish_non_exhaustive()
    }
}
