//! The read side on a completion driver: the driver's part as operations,
//! one out at a time, and their completions.
//!
//! What an operation brings goes where the readiness driver's own calls put
//! it: a stream's bytes into the read-ahead (or into the lent buffer), a
//! listener's connection into the accepted ones, a datagram with its
//! sender. While one is out the side asks for nothing more; its completion
//! clears that, and wakes the tasks waiting, which take what came or ask
//! again. Only the operation's own completion clears it, so a side never
//! has two operations out, and its mailbox never more than one completion.
//!
//! On a port a TCP read the pool has no buffer for waits in a zero-byte
//! receive, which holds none; once that says bytes came, a receive takes a
//! buffer for them, or, with the pool still empty, the task reads them into
//! its own (`read_starved`). So no set of idle peers or stopped readers keeps
//! a read from its bytes (proofs/tla/port/PortStarve.tla).

use super::{Datagram, Kind, Peer, Piece, ReadState, UDP_MAX};
use crate::io::raw::Raw;
use crate::io::side::After;
use crate::io::source::Io;
use crate::io::source::ops::{Answer, Done, Outcome, Want};
use crate::io::source::route::Route;
use crate::io::sys;

impl ReadState {
    /// Issues the read the waiting tasks want, if any wait, where this
    /// thread reaches the driver.
    pub(crate) fn submit_wanted(&mut self, io: Io<'_>, kind: Kind, after: &mut After) {
        if !self.waiters.is_empty() || self.lent_waits() {
            self.complete(io, kind, self.want, after);
            // Bytes came and the pool has no buffer for them: the tasks read
            // them into their own (`read_starved`).
            #[cfg(port)]
            if self.starved && self.readable && self.out.is_none() {
                after.wake(&mut self.waiters);
            }
        }
    }

    /// Stops the multishot receive out, once: a task takes the next bytes
    /// itself, and its end wakes the tasks.
    #[cfg(ring)]
    pub(super) fn stop_multi(&mut self, route: Route<'_>) {
        if let Some(id) = self.out
            && self.multi
            && !self.multi_stopping
        {
            self.multi_stopping = true;
            route.cancel(id);
        }
    }

    /// The driver's part on a completion driver: issues the read the
    /// waiting tasks need (`len` asked), unless one is out or a result waits
    /// to be taken. A route that cannot issue here leaves it to the thread
    /// that can.
    pub(super) fn submit_ops(
        &mut self,
        route: Route<'_>,
        kind: Kind,
        len: usize,
        after: &mut After,
    ) {
        // A lent buffer that waits with nothing read ahead takes the next
        // bytes itself: a multishot receive out is stopped for it.
        #[cfg(ring)]
        if self.ahead.is_empty()
            && self
                .lent
                .as_ref()
                .is_some_and(|l| !l.out && l.done.is_none())
        {
            self.stop_multi(route);
        }
        // The caller's own calls: only a poll, while the side is not ready
        // and a task waits. A receive of the driver's still out is stopped
        // first; what it brings comes before the caller's calls.
        #[cfg(ring)]
        if self.user_fd {
            if let Some(id) = self.out {
                if !self.polling && !self.stopping {
                    self.stopping = true;
                    self.multi_stopping = self.multi;
                    route.cancel(id);
                }
                return;
            }
            if self.waiters.is_empty() {
                return;
            }
            if !self.blocked {
                // The receive stopped with the side ready.
                after.wake(&mut self.waiters);
            } else if let Some(issuer) = route.issuer(after) {
                self.polling = true;
                self.out = Some(issuer.issue(Want::Ready { read: true }, kind, self.epoch, after));
            }
            return;
        }
        if self.out.is_some() || self.has_result() || self.lent_done() {
            return;
        }
        let Some(issuer) = route.issuer(after) else {
            return;
        };
        // Only a ring gives an operation back because the socket was not
        // ready: the next one waits for it to be.
        #[cfg(ring)]
        if self.poll_first {
            self.out = Some(issuer.issue(Want::Ready { read: true }, kind, self.epoch, after));
            return;
        }
        let want = if kind.listens() {
            #[cfg(ring)]
            if self.streams {
                self.multi = true;
                Want::AcceptMulti
            } else {
                Want::Accept
            }
            #[cfg(not(ring))]
            Want::Accept
        } else if kind.datagrams() && self.lent.as_ref().is_some_and(|l| !l.out) {
            let Some(lent) = self.lent.as_mut() else {
                return;
            };
            lent.out = true;
            Want::RecvLentFrom(core::mem::take(&mut lent.buf))
        } else if kind.datagrams() {
            // A Unix datagram is its task's to receive into its own buffer
            // (`receive_now`): the ring only polls for it.
            #[cfg(ring)]
            if kind == Kind::UnixDatagram {
                return;
            }
            #[cfg(ring)]
            let multi = !self.dry && issuer.multishot_datagrams(kind);
            #[cfg(not(ring))]
            let multi = false;
            match self.datagram_want(multi, len, after) {
                Some(want) => want,
                // No buffer of the pool: a poll that holds none, and the task
                // receives into its own buffer (`task_receives`).
                #[cfg(ring)]
                None if self.starved && !self.readable && self.error.is_none() => {
                    Want::Ready { read: true }
                }
                None => return,
            }
        } else {
            match self.lent.as_mut().filter(|l| !l.out) {
                Some(lent) => {
                    lent.out = true;
                    Want::RecvLent(core::mem::take(&mut lent.buf))
                }
                #[cfg(ring)]
                None if !self.dry && issuer.multishot(kind) => {
                    self.multi = true;
                    Want::RecvMulti
                }
                None => match self.buffer(len, false, after) {
                    Some(buf) => Want::Recv(buf),
                    // No buffer of the pool: rather than wait for one that a
                    // receive of another connection may hold for good, a poll
                    // that holds none, and the task reads into its own buffer
                    // (`reads_itself`).
                    #[cfg(ring)]
                    None if self.starved && !self.readable => Want::Ready { read: true },
                    // On a port a zero-byte receive, which holds none, and once it
                    // says bytes came the task reads them into its own buffer
                    // (`read_starved`, proofs/tla/port/PortStarve.tla).
                    #[cfg(port)]
                    None if kind == Kind::Tcp && !self.readable => Want::Ready { read: true },
                    None => return,
                },
            }
        };
        // The bytes the zero-byte receive said came are this receive's.
        #[cfg(port)]
        if matches!(want, Want::Recv(_) | Want::RecvLent(_)) {
            self.readable = false;
        }
        self.out = Some(issuer.issue(want, kind, self.epoch, after));
    }

    /// The datagram receive to go out: a multishot one into the receive
    /// group (`multi`), or one into a buffer of the pool that holds `len`
    /// whole; `None` while the pool has none.
    fn datagram_want(&mut self, multi: bool, len: usize, after: &mut After) -> Option<Want> {
        #[cfg(ring)]
        if multi {
            self.multi = true;
            return Some(Want::RecvFromMulti);
        }
        #[cfg(not(ring))]
        let _ = multi;
        // Only UDP is received ahead, and no UDP datagram is longer than its
        // length field counts: a larger buffer of the task's needs no larger
        // one of the pool.
        self.buffer(len.min(UDP_MAX), true, after)
            .map(Want::RecvFrom)
    }

    /// A receive into a buffer of the pool completed.
    fn recv_done(&mut self, buf: crate::buf::PoolBuf, answer: Answer<usize>, after: &mut After) {
        match answer {
            Answer::Undone { not_ready } => {
                self.not_ready(not_ready);
                after.drop_buf(buf);
            }
            Answer::Done(Ok(0)) => {
                self.eof = true;
                after.drop_buf(buf);
            }
            Answer::Done(Ok(_)) => self.ahead.push(Piece::Pool(buf), after),
            Answer::Done(Err(err)) => {
                self.error = Some(err);
                after.drop_buf(buf);
            }
        }
    }

    /// The kernel gave an operation back because the socket was not ready.
    fn not_ready(&mut self, not_ready: bool) {
        #[cfg(ring)]
        {
            self.poll_first |= not_ready;
        }
        #[cfg(not(ring))]
        let _ = not_ready;
    }

    /// The side's operation completed: what it brought goes where the tasks
    /// take it, and they are woken. One given back undone brought nothing,
    /// and is asked again. `fd` is the source's descriptor (a listener's,
    /// for the accepted connection to take its context from).
    pub(crate) fn op_done(&mut self, done: Done, fd: Option<Raw>, after: &mut After) {
        self.out = None;
        #[cfg(ring)]
        {
            self.polling = false;
            self.stopping = false;
        }
        // An operation of a descriptor that is gone: what it holds is
        // dropped.
        let stale = done.epoch != self.epoch;
        match done.outcome {
            #[cfg(ring)]
            Outcome::Ready(answer) => {
                if !stale && let Answer::Done(result) = answer {
                    self.poll_first = false;
                    if let Err(err) = result {
                        self.error = Some(err);
                    }
                    self.readable = self.starved;
                    if core::mem::take(&mut self.task_polls) || self.starved {
                        after.wake(&mut self.waiters);
                    }
                    if self.user_fd {
                        self.blocked = false;
                        self.edges = self.edges.wrapping_add(1);
                        after.wake(&mut self.waiters);
                    }
                }
            }
            // Bytes or the end came: the receive into a buffer of the pool goes
            // out at once (`submit_wanted`), and its end wakes the tasks.
            #[cfg(port)]
            Outcome::Ready(answer) => {
                if !stale && let Answer::Done(result) = answer {
                    match result {
                        Ok(()) => {
                            self.readable = true;
                            return;
                        }
                        Err(err) => self.error = Some(err),
                    }
                }
            }
            #[cfg(ring)]
            Outcome::RecvMultiEnd { answer, dry } => {
                self.multi = false;
                self.multi_stopping = false;
                if !stale {
                    // The next receive takes a buffer of the pool if the
                    // group ran dry.
                    self.dry = dry;
                    match answer {
                        Answer::Done(Ok(0)) => self.eof = true,
                        // Its bytes came as arrivals already.
                        Answer::Done(Ok(_)) => {}
                        Answer::Done(Err(err)) => self.error = Some(err),
                        // Stopped, or the group ran dry: asked again.
                        Answer::Undone { not_ready } => self.not_ready(not_ready),
                    }
                }
            }
            Outcome::Recv(buf, _) if stale => after.drop_buf(buf),
            Outcome::Recv(buf, answer) => {
                // The group may have buffers again: the next receive tries.
                #[cfg(ring)]
                {
                    self.dry = false;
                }
                self.recv_done(buf, answer, after);
            }
            Outcome::RecvLent(buf, answer) => {
                if let Some(lent) = self.lent.as_mut().filter(|l| l.out && !stale) {
                    lent.buf = buf;
                    lent.out = false;
                    match answer {
                        Answer::Undone { not_ready } => {
                            self.not_ready(not_ready);
                        }
                        Answer::Done(result) => lent.done = Some(result),
                    }
                    let Some(lent) = self.lent.as_mut() else {
                        return;
                    };
                    if lent.orphan {
                        // Its task went while the buffer was out: what came
                        // is the read-ahead now.
                        self.abandon_lent(after);
                    } else if lent.done.is_some()
                        && let Some(w) = lent.waker.take()
                    {
                        after.wake_one(w);
                    }
                }
            }
            Outcome::RecvLentFrom(buf, answer) => {
                if let Some(lent) = self.lent.as_mut().filter(|l| l.out && !stale) {
                    lent.buf = buf;
                    lent.out = false;
                    match answer {
                        Answer::Undone { not_ready } => self.not_ready(not_ready),
                        Answer::Done(Ok((n, from))) => {
                            lent.done = Some(Ok(n));
                            lent.peer = Some(from);
                        }
                        Answer::Done(Err(err)) => lent.done = Some(Err(err)),
                    }
                    let Some(lent) = self.lent.as_mut() else {
                        return;
                    };
                    if lent.orphan {
                        self.abandon_lent(after);
                    } else if lent.done.is_some()
                        && let Some(w) = lent.waker.take()
                    {
                        after.wake_one(w);
                    }
                }
            }
            #[cfg(ring)]
            Outcome::AcceptMultiEnd(answer) => {
                self.multi = false;
                self.multi_stopping = false;
                if !stale {
                    match answer {
                        // Its connections came as arrivals.
                        Answer::Done(Ok(())) => {}
                        Answer::Done(Err(err)) => self.error = Some(err),
                        Answer::Undone { not_ready } => self.not_ready(not_ready),
                    }
                }
            }
            Outcome::Accept(_) if stale => {}
            Outcome::Accept(Answer::Undone { not_ready }) => self.not_ready(not_ready),
            Outcome::Accept(Answer::Done(Ok((conn, peer)))) => {
                match fd.map_or(Ok(()), |listener| sys::adopt_accepted(listener, &conn)) {
                    Ok(()) => self.accepted.push_back((conn, Peer::Known(peer))),
                    Err(err) => self.error = Some(err),
                }
            }
            Outcome::Accept(Answer::Done(Err(err))) => self.error = Some(err),
            Outcome::RecvFrom(buf, _) if stale => after.drop_buf(buf),
            Outcome::RecvFrom(buf, answer) => {
                // The group may have buffers again: the next receive tries.
                #[cfg(ring)]
                {
                    self.dry = false;
                }
                match answer {
                    Answer::Undone { not_ready } => {
                        self.not_ready(not_ready);
                        after.drop_buf(buf);
                    }
                    Answer::Done(Ok((_, from))) => {
                        self.datagrams.push_back((Datagram::Pool(buf), from));
                    }
                    Answer::Done(Err(err)) => {
                        self.error = Some(err);
                        after.drop_buf(buf);
                    }
                }
            }
            // The write side's.
            other => other.release(after),
        }
        // The tasks waiting take what came, or ask again; a lent buffer
        // that waits takes it first.
        self.want = 0;
        if self.has_result()
            && self.lent_idle()
            && let Some(w) = self.lent.as_mut().and_then(|l| l.waker.take())
        {
            after.wake_one(w);
        }
        after.wake(&mut self.waiters);
    }
}
