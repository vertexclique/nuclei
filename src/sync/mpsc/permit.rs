//! Reserved capacity of the bounded channel: [`Permit`], [`PermitIterator`]
//! and [`OwnedPermit`].
//!
//! A permit stands for one slot of the channel that its holder took out of
//! the semaphore. Sending through it pushes the value into the slot, which the
//! receiver gives back when it takes the value; dropping it unused gives the
//! slot back at once. Either way the receiver is woken if the slot's return
//! leaves a closed channel idle (see `Chan::give_back`).

#![allow(unsafe_code)]

use core::fmt;
use core::iter::FusedIterator;
use core::mem::{self, ManuallyDrop};
use core::ptr;

use super::bounded::{Bounded, Sender};
use super::chan::Chan;

/// Permits to send one value into the channel.
///
/// Returned by [`Sender::reserve`] and [`Sender::try_reserve`]; it guarantees
/// room for one message before the message exists. The send through it never
/// waits and never fails, even when the receiver was closed in the meantime
/// (see [`Receiver::close`](super::Receiver::close)). Dropping it without
/// sending gives the slot back.
///
/// # Progress
///
/// [`send`](Self::send) is lock-free, not wait-free: it pushes onto the
/// message queue, which retries when it loses a slot to another sender.
/// Dropping the permit is one atomic add on the semaphore when no sender
/// waits, and lock-free when it serves waiting senders.
///
/// ```
/// use nuclei::sync::mpsc;
///
/// let (tx, mut rx) = mpsc::channel(1);
/// let permit = tx.try_reserve().unwrap();
/// permit.send("reserved");
/// assert_eq!(rx.try_recv(), Ok("reserved"));
/// ```
#[must_use = "dropping a permit gives its slot back unused"]
pub struct Permit<'a, T> {
    chan: &'a Chan<T, Bounded>,
}

impl<'a, T> Permit<'a, T> {
    pub(super) fn new(chan: &'a Chan<T, Bounded>) -> Self {
        Self { chan }
    }

    /// Sends a value through the reserved slot, consuming the permit.
    ///
    /// The send succeeds even if the receiver was closed after the slot was
    /// reserved: closing keeps what is reserved receivable, so a clean
    /// shutdown loses nothing. (If the receiver was dropped, the value is
    /// dropped when the last sender is.)
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel(1);
    /// let permit = tx.try_reserve().unwrap();
    /// rx.close();
    /// permit.send(456);
    /// assert_eq!(rx.try_recv(), Ok(456));
    /// ```
    pub fn send(self, value: T) {
        let chan = self.chan;
        // The slot goes to the value, not back to the semaphore.
        mem::forget(self);
        chan.send(value);
    }
}

impl<T> Drop for Permit<'_, T> {
    fn drop(&mut self) {
        self.chan.give_back(1);
    }
}

impl<T> fmt::Debug for Permit<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit").finish_non_exhaustive()
    }
}

/// An [`Iterator`] of [`Permit`]s that holds `n` slots of the channel.
///
/// Returned by [`Sender::reserve_many`] and [`Sender::try_reserve_many`]. Each
/// item is one permit; dropping the iterator before it is exhausted gives the
/// remaining slots back. It knows its length.
///
/// # Progress
///
/// `next`, `len` and `size_hint` touch no shared state and are wait-free.
/// Dropping the iterator returns all its remaining slots with one semaphore
/// release: one atomic add when no sender waits, lock-free when it serves
/// waiting senders.
///
/// ```
/// use nuclei::sync::mpsc;
///
/// let (tx, mut rx) = mpsc::channel(3);
/// let mut permits = tx.try_reserve_many(3).unwrap();
/// assert_eq!(permits.len(), 3);
/// permits.next().unwrap().send(1);
/// assert_eq!(tx.capacity(), 0);
/// drop(permits);
/// assert_eq!(tx.capacity(), 2, "the two unused slots came back");
/// assert_eq!(rx.try_recv(), Ok(1));
/// ```
#[must_use = "dropping the iterator gives its slots back unused"]
pub struct PermitIterator<'a, T> {
    chan: &'a Chan<T, Bounded>,
    n: usize,
}

impl<'a, T> PermitIterator<'a, T> {
    pub(super) fn new(chan: &'a Chan<T, Bounded>, n: usize) -> Self {
        Self { chan, n }
    }
}

impl<'a, T> Iterator for PermitIterator<'a, T> {
    type Item = Permit<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.n == 0 {
            return None;
        }
        self.n -= 1;
        Some(Permit { chan: self.chan })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.n, Some(self.n))
    }
}

impl<T> ExactSizeIterator for PermitIterator<'_, T> {}

impl<T> FusedIterator for PermitIterator<'_, T> {}

impl<T> Drop for PermitIterator<'_, T> {
    fn drop(&mut self) {
        if self.n > 0 {
            self.chan.give_back(self.n);
        }
    }
}

impl<T> fmt::Debug for PermitIterator<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermitIterator")
            .field("remaining", &self.n)
            .finish()
    }
}

/// An owned permit to send one value into the channel.
///
/// The same as [`Permit`], except that it holds the [`Sender`] it was reserved
/// from instead of borrowing it, so it has no lifetime. Returned by
/// [`Sender::reserve_owned`] and [`Sender::try_reserve_owned`]. It counts as a
/// sender while it lives, so the channel does not end for the receiver while a
/// permit is held. [`send`](Self::send) and [`release`](Self::release) hand
/// the sender back; dropping the permit unused gives the slot back and drops
/// the sender.
///
/// # Progress
///
/// [`send`](Self::send) is lock-free, not wait-free, for the reason
/// [`Permit::send`] is: it pushes onto the message queue. [`release`](Self::release)
/// and the drop return the slot as dropping a [`Permit`] does (one atomic add
/// when no sender waits, lock-free otherwise), and the drop then counts the
/// sender out, which is wait-free. [`same_channel`](Self::same_channel) and
/// [`same_channel_as_sender`](Self::same_channel_as_sender) are wait-free.
///
/// ```
/// use nuclei::sync::mpsc;
///
/// let (tx, mut rx) = mpsc::channel(1);
/// let permit = tx.try_reserve_owned().unwrap();
/// let tx = permit.send(123);
/// assert_eq!(rx.try_recv(), Ok(123));
/// tx.try_send(456).unwrap();
/// ```
#[must_use = "dropping a permit gives its slot back unused"]
pub struct OwnedPermit<T> {
    sender: Sender<T>,
}

impl<T> OwnedPermit<T> {
    pub(super) fn new(sender: Sender<T>) -> Self {
        Self { sender }
    }

    /// The sender, moved out of a permit that is consumed without running its
    /// drop (which would give the slot back).
    fn into_sender(self) -> Sender<T> {
        let this = ManuallyDrop::new(self);
        // SAFETY: `this` is never used or dropped again, so the sender is
        // moved out exactly once, and the permit's `Drop` (which would return
        // the slot) never runs.
        unsafe { ptr::read(&this.sender) }
    }

    /// Sends a value through the reserved slot, returning the sender the
    /// permit was reserved from.
    ///
    /// As for [`Permit::send`], it succeeds even if the receiver was closed
    /// after the reservation.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, mut rx) = mpsc::channel(1);
    /// let permit = tx.try_reserve_owned().unwrap();
    /// let tx = permit.send(456);
    /// assert_eq!(rx.try_recv(), Ok(456));
    /// tx.try_send(789).unwrap();
    /// ```
    pub fn send(self, value: T) -> Sender<T> {
        let sender = self.into_sender();
        sender.chan.send(value);
        sender
    }

    /// Gives the reserved slot back without sending, returning the sender.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, _rx) = mpsc::channel(1);
    /// let permit = tx.clone().try_reserve_owned().unwrap();
    /// assert!(tx.try_send(123).is_err(), "the permit holds the only slot");
    /// let tx2 = permit.release();
    /// tx.try_send(789).unwrap();
    /// drop(tx2);
    /// ```
    pub fn release(self) -> Sender<T> {
        let sender = self.into_sender();
        sender.chan.give_back(1);
        sender
    }

    /// Whether two permits belong to the same channel.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, _rx) = mpsc::channel::<()>(2);
    /// let a = tx.clone().try_reserve_owned().unwrap();
    /// let b = tx.clone().try_reserve_owned().unwrap();
    /// assert!(a.same_channel(&b));
    ///
    /// let (tx2, _rx2) = mpsc::channel::<()>(1);
    /// let c = tx2.try_reserve_owned().unwrap();
    /// assert!(!c.same_channel(&a));
    /// ```
    pub fn same_channel(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
    }

    /// Whether this permit belongs to the same channel as `sender`.
    ///
    /// ```
    /// use nuclei::sync::mpsc;
    ///
    /// let (tx, _rx) = mpsc::channel::<()>(1);
    /// let permit = tx.clone().try_reserve_owned().unwrap();
    /// assert!(permit.same_channel_as_sender(&tx));
    ///
    /// let (tx2, _rx2) = mpsc::channel::<()>(1);
    /// assert!(!permit.same_channel_as_sender(&tx2));
    /// ```
    pub fn same_channel_as_sender(&self, sender: &Sender<T>) -> bool {
        self.sender.same_channel(sender)
    }
}

impl<T> Drop for OwnedPermit<T> {
    fn drop(&mut self) {
        // The sender field drops after this, which counts the sender out.
        self.sender.chan.give_back(1);
    }
}

impl<T> fmt::Debug for OwnedPermit<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedPermit").finish_non_exhaustive()
    }
}
