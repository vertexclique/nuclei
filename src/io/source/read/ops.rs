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

use super::{Kind, Piece, ReadState};
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
        if let Some(id) = self.out
            && self.multi
            && !self.multi_stopping
            && self.ahead.is_empty()
            && self
                .lent
                .as_ref()
                .is_some_and(|l| !l.out && l.done.is_none())
        {
            self.multi_stopping = true;
            route.cancel(id);
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
            Want::Accept
        } else if kind.datagrams() {
            let Some(buf) = self.buffer(len, true, after) else {
                return;
            };
            Want::RecvFrom(buf)
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
                None => {
                    let Some(buf) = self.buffer(len, false, after) else {
                        return;
                    };
                    Want::Recv(buf)
                }
            }
        };
        self.out = Some(issuer.issue(want, kind, self.epoch, after));
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
            Outcome::Accept(_) if stale => {}
            Outcome::Accept(Answer::Undone { not_ready }) => self.not_ready(not_ready),
            Outcome::Accept(Answer::Done(Ok((conn, peer)))) => {
                match fd.map_or(Ok(()), |listener| sys::adopt_accepted(listener, &conn)) {
                    Ok(()) => self.accepted.push_back((conn, peer)),
                    Err(err) => self.error = Some(err),
                }
            }
            Outcome::Accept(Answer::Done(Err(err))) => self.error = Some(err),
            Outcome::RecvFrom(buf, _) if stale => after.drop_buf(buf),
            Outcome::RecvFrom(buf, answer) => match answer {
                Answer::Undone { not_ready } => {
                    self.not_ready(not_ready);
                    after.drop_buf(buf);
                }
                Answer::Done(Ok((_, from))) => self.datagrams.push_back((buf, from)),
                Answer::Done(Err(err)) => {
                    self.error = Some(err);
                    after.drop_buf(buf);
                }
            },
            // The write side's.
            other => other.release(after),
        }
        // The tasks waiting take what came, or ask again.
        self.want = 0;
        after.wake(&mut self.waiters);
    }
}
