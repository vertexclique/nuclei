//! A full mesh of bounded channels between a fixed number of parties.
//!
//! [`mesh`] builds, for every ordered pair `(from, to)` of parties, one
//! [`mpsc::channel`] of the given capacity, and hands each party its ends: a
//! [`Party`] keeps the sender to every party at the index of that party, and
//! the receiver from every party at the index of that party. Parties are
//! numbered `0..parties`. Self-loops are included: party `i` can send to
//! itself, so "send to the owner of key `k`" needs no special case.
//!
//! The mesh only builds channels. Nothing here spawns anything or knows about
//! workers: moving each [`Party`] into its own task or thread (a `Party` is
//! `Send` when `T` is) is the caller's, with any `spawn`.
//!
//! Backpressure is per pair: a send to a party whose channel from this one is
//! full waits for that party to take a value, and does not hold up sends to
//! other parties or from other parties.
//!
//! [`Party::recv`] receives from any party and is fair: it polls the
//! receivers round robin, starting after the one that delivered last, so a
//! party that floods cannot starve a quiet one. A party's own sender to itself
//! keeps its self-loop open, so `recv` ends only after
//! [`Party::close_senders`] too, once every other party dropped its senders and
//! everything sent was received.
//!
//! ```
//! use nuclei::sync::mesh::mesh;
//!
//! nuclei::runtime::block_on(async {
//!     let mut parties = mesh::<u32>(2, 4);
//!     let mut b = parties.pop().unwrap();
//!     let a = parties.pop().unwrap();
//!     a.send_to(1, 7).await.unwrap();
//!     assert_eq!(b.recv().await, Some((0, 7)));
//! });
//! ```

use core::fmt;
use core::future::poll_fn;
use core::task::{Context, Poll};

use crate::sync::mpsc::{
    self,
    error::{SendError, TryRecvError, TrySendError},
};

/// Builds a full mesh of `parties` parties, each pair joined by a bounded
/// channel of `capacity` messages in each direction. Entry `i` of the answer
/// is party `i`.
///
/// # Panics
///
/// When `capacity` is zero, as [`mpsc::channel`] does.
///
/// ```
/// let parties = nuclei::sync::mesh::mesh::<u8>(3, 1);
/// assert_eq!(parties.len(), 3);
/// assert_eq!(parties[2].index(), 2);
/// assert_eq!(parties[2].parties(), 3);
/// ```
#[track_caller]
pub fn mesh<T: Send>(parties: usize, capacity: usize) -> Vec<Party<T>> {
    assert!(capacity > 0, "mpsc bounded channel requires buffer > 0");
    let mut senders: Vec<Vec<mpsc::Sender<T>>> =
        (0..parties).map(|_| Vec::with_capacity(parties)).collect();
    let mut receivers: Vec<Vec<mpsc::Receiver<T>>> =
        (0..parties).map(|_| Vec::with_capacity(parties)).collect();
    for sending in &mut senders {
        for receiving in &mut receivers {
            let (tx, rx) = mpsc::channel(capacity);
            sending.push(tx);
            receiving.push(rx);
        }
    }
    senders
        .into_iter()
        .zip(receivers)
        .enumerate()
        .map(|(index, (senders, receivers))| Party {
            index,
            parties,
            ended: vec![false; receivers.len()],
            ended_count: 0,
            next: 0,
            senders,
            receivers,
        })
        .collect()
}

/// One party of a [`mesh`]: its senders to every party and its receivers from
/// every party, both indexed by the peer.
pub struct Party<T> {
    index: usize,
    parties: usize,
    senders: Vec<mpsc::Sender<T>>,
    receivers: Vec<mpsc::Receiver<T>>,
    ended: Vec<bool>,
    ended_count: usize,
    next: usize,
}

impl<T> Party<T> {
    /// This party's index in the mesh.
    pub fn index(&self) -> usize {
        self.index
    }

    /// How many parties the mesh has, this one included.
    pub fn parties(&self) -> usize {
        self.parties
    }

    /// The sender to party `to`, or `None` when `to` is not a party (or the
    /// senders were closed with [`close_senders`](Self::close_senders)).
    pub fn sender(&self, to: usize) -> Option<&mpsc::Sender<T>> {
        self.senders.get(to)
    }

    /// The receiver from party `from`, or `None` when `from` is not a party.
    pub fn receiver(&mut self, from: usize) -> Option<&mut mpsc::Receiver<T>> {
        self.receivers.get_mut(from)
    }

    /// Sends `value` to party `to`, waiting for room in the channel to it.
    ///
    /// # Errors
    ///
    /// A [`SendError`] holding the value when `to` is not a party or its
    /// receiver is gone.
    pub async fn send_to(&self, to: usize, value: T) -> Result<(), SendError<T>> {
        match self.senders.get(to) {
            Some(sender) => sender.send(value).await,
            None => Err(SendError(value)),
        }
    }

    /// Sends `value` to party `to` at once, or says why it cannot be sent.
    ///
    /// # Errors
    ///
    /// [`TrySendError::Full`] when the channel to `to` is full;
    /// [`TrySendError::Closed`] when `to` is not a party or its receiver is
    /// gone. Both hold the value back.
    pub fn try_send_to(&self, to: usize, value: T) -> Result<(), TrySendError<T>> {
        match self.senders.get(to) {
            Some(sender) => sender.try_send(value),
            None => Err(TrySendError::Closed(value)),
        }
    }

    /// Drops every sender of this party, itself included (the self-loop). The
    /// party then only receives; its sends fail with their value. This is how
    /// a party lets [`recv`](Self::recv) end.
    pub fn close_senders(&mut self) {
        self.senders.clear();
    }

    /// Receives the next value from party `from`. Answers `None` when `from`
    /// is not a party, or that channel ended and was drained.
    pub async fn recv_from(&mut self, from: usize) -> Option<T> {
        match self.receivers.get_mut(from) {
            Some(receiver) => receiver.recv().await,
            None => None,
        }
    }

    /// Receives the next value from any party, with the index of the party
    /// that sent it. Answers `None` once every channel into this party ended
    /// and was drained.
    ///
    /// Cancel safe: a dropped future loses no value.
    pub async fn recv(&mut self) -> Option<(usize, T)> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Polls for a value from any party. The receivers are polled round
    /// robin from the one after the last that delivered, so no party is
    /// starved, and each one that has nothing registers the waker of `cx`.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<(usize, T)>> {
        let n = self.receivers.len();
        for step in 0..n {
            let from = (self.next + step) % n;
            if self.ended[from] {
                continue;
            }
            match self.receivers[from].poll_recv(cx) {
                Poll::Ready(Some(value)) => {
                    self.next = (from + 1) % n;
                    return Poll::Ready(Some((from, value)));
                }
                Poll::Ready(None) => self.end(from),
                Poll::Pending => {}
            }
        }
        if self.ended_count == n {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    /// Receives a value from any party without waiting, in the order
    /// [`poll_recv`](Self::poll_recv) uses.
    ///
    /// # Errors
    ///
    /// [`TryRecvError::Empty`] when nothing is waiting and a value may yet
    /// come; [`TryRecvError::Disconnected`] only when every channel ended and
    /// was drained.
    pub fn try_recv(&mut self) -> Result<(usize, T), TryRecvError> {
        let n = self.receivers.len();
        for step in 0..n {
            let from = (self.next + step) % n;
            if self.ended[from] {
                continue;
            }
            match self.receivers[from].try_recv() {
                Ok(value) => {
                    self.next = (from + 1) % n;
                    return Ok((from, value));
                }
                Err(TryRecvError::Disconnected) => self.end(from),
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.ended_count == n {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn end(&mut self, from: usize) {
        self.ended[from] = true;
        self.ended_count += 1;
    }

    /// Takes the party apart: its senders and its receivers, indexed by peer.
    pub fn into_parts(self) -> (Vec<mpsc::Sender<T>>, Vec<mpsc::Receiver<T>>) {
        (self.senders, self.receivers)
    }
}

impl<T> fmt::Debug for Party<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Party")
            .field("index", &self.index)
            .field("parties", &self.parties)
            .finish()
    }
}
