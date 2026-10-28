//! The write side on a completion driver: the driver's part as operations,
//! one out at a time, and their completions.
//!
//! The operations go in the order the readiness driver makes its calls: a
//! connect, then the stream's bytes (what the driver gave back first, then
//! the write-behind, then the lent buffer), or a socket's datagrams in the
//! order they were queued. The write-behind moves into its operation, so
//! writes made while it is out start a new one behind it. The kernel may
//! take part of a stream's bytes: the rest goes out next, ahead of
//! anything written after. One the kernel gives back undone (cancelled as
//! the source moved, or with the socket not ready) took none of its bytes
//! and goes out again.

use std::io;

use super::{Kind, Lent, WriteState};
use crate::io::raw::Raw;
use crate::io::side::After;
use crate::io::source::ops::{Answer, Done, Outcome, Want};
use crate::io::source::route::Route;
use crate::io::source::{Failure, Io};
use crate::io::sys;

impl WriteState {
    /// What the side holds for the driver: a connect, bytes, a lent buffer
    /// not out yet, or a datagram.
    fn op_work(&self) -> bool {
        self.dial.is_some()
            || self.again.is_some()
            || self.behind.is_some()
            || self
                .lent
                .as_ref()
                .is_some_and(|l| l.done.is_none() && !l.out)
            || self.queued.is_some()
    }

    /// Issues what the side holds, if this thread reaches a completion
    /// driver.
    #[cfg_attr(
        not(readiness),
        expect(irrefutable_let_patterns, reason = "a port is the only driver there")
    )]
    pub(crate) fn submit_wanted(&mut self, io: Io<'_>, kind: Kind, after: &mut After) {
        if let Io::Ops(route) = io {
            self.submit_ops(route, kind, after);
        }
    }

    /// The driver's part on a completion driver: issues the side's next
    /// operation, unless one is out. A route that cannot issue here leaves
    /// it to the thread that can.
    pub(super) fn submit_ops(&mut self, route: Route<'_>, kind: Kind, after: &mut After) {
        if self.out.is_some() {
            return;
        }
        // A caller's descriptor: only a poll, while it is not ready and a
        // task waits.
        #[cfg(ring)]
        if self.user_fd {
            if self.blocked
                && !self.waiters.is_empty()
                && let Some(issuer) = route.issuer(after)
            {
                self.out = Some(issuer.issue(Want::Ready { read: false }, kind, self.epoch, after));
            }
            return;
        }
        if let Some(failure) = self.failed
            && let Some(lent) = self.lent.as_mut().filter(|l| l.done.is_none() && !l.out)
        {
            // The write-behind before it failed: none of it goes out.
            lent.done = Some(Err(failure.error()));
            self.lent_finished(after);
        }
        if !self.not_ready_first() && !self.op_work() {
            return;
        }
        let Some(issuer) = route.issuer(after) else {
            return;
        };
        #[cfg(ring)]
        if self.poll_first {
            self.out = Some(issuer.issue(Want::Ready { read: false }, kind, self.epoch, after));
            return;
        }
        let want = if let Some(dial) = self.dial.take() {
            Want::Connect(dial)
        } else if let Some(want) = self.again.take() {
            want
        } else if let Some(buf) = self.behind.take() {
            Want::Send(buf)
        } else if let Some(lent) = self.lent.as_mut().filter(|l| l.done.is_none() && !l.out) {
            lent.out = true;
            Want::SendLent(core::mem::take(&mut lent.buf), lent.sent)
        } else if let Some((buf, target)) = self.queued.take() {
            Want::SendTo(buf, target)
        } else {
            return;
        };
        self.out = Some(issuer.issue(want, kind, self.epoch, after));
    }

    /// Whether the next operation waits for the socket to be ready first.
    fn not_ready_first(&self) -> bool {
        #[cfg(ring)]
        return self.poll_first;
        #[cfg(not(ring))]
        false
    }

    /// The kernel gave an operation back because the socket was not ready
    /// (or, for a connect, because it is under way).
    fn not_ready(&mut self, not_ready: bool) {
        #[cfg(ring)]
        {
            self.poll_first |= not_ready;
        }
        #[cfg(not(ring))]
        let _ = not_ready;
    }

    /// The side's operation completed: the bytes the kernel took are gone,
    /// the rest goes out next, and the tasks waiting are woken. `fd` is the
    /// source's descriptor, for a connect's end.
    pub(crate) fn op_done(&mut self, done: Done, fd: Option<Raw>, after: &mut After) {
        self.out = None;
        // An operation of a descriptor that is gone: what it holds is
        // dropped.
        let stale = done.epoch != self.epoch;
        match done.outcome {
            Outcome::Send(buf, _) if stale => after.drop_buf(buf),
            Outcome::SendTo(buf, ..) if stale => after.drop_buf(buf),
            Outcome::Send(buf, Answer::Undone { not_ready }) => {
                self.not_ready(not_ready);
                self.again = Some(Want::Send(buf));
            }
            Outcome::SendTo(buf, target, Answer::Undone { not_ready }) => {
                self.not_ready(not_ready);
                self.again = Some(Want::SendTo(buf, target));
            }
            Outcome::Send(mut buf, Answer::Done(result)) => match result {
                Ok(n) if n > 0 => {
                    buf.consume(n);
                    if buf.is_empty() {
                        after.drop_buf(buf);
                    } else {
                        self.again = Some(Want::Send(buf));
                    }
                }
                other => {
                    let err = other
                        .err()
                        .unwrap_or_else(|| io::ErrorKind::WriteZero.into());
                    self.failed = Some(Failure::of(&err));
                    after.drop_buf(buf);
                    // What was written behind it never goes out.
                    if let Some(behind) = self.behind.take() {
                        after.drop_buf(behind);
                    }
                }
            },
            Outcome::SendTo(buf, _, Answer::Done(result)) => {
                if let Err(err) = result {
                    self.error = Some(err);
                }
                after.drop_buf(buf);
            }
            Outcome::SendLent(buf, answer) => self.lent_back(buf, answer, stale, after),
            Outcome::Connect(..) if stale => {}
            // Given back as the source moved: asked again.
            Outcome::Connect(dial, Answer::Undone { not_ready: false }) => self.dial = Some(dial),
            // Under way: its end is seen once the socket is ready.
            Outcome::Connect(_, Answer::Undone { not_ready: true }) => self.not_ready(true),
            Outcome::Connect(_, Answer::Done(result)) => {
                let result = result.and_then(|()| fd.map_or(Ok(()), sys::adopt_connected));
                self.connected(result);
            }
            #[cfg(ring)]
            Outcome::Ready(answer) => {
                if !stale && let Answer::Done(_) = answer {
                    self.poll_first = false;
                    if self.user_fd {
                        self.blocked = false;
                        self.edges = self.edges.wrapping_add(1);
                    }
                    if self.connecting {
                        // A connect under way ended: the socket says how.
                        self.connected(match fd.map(sys::take_error) {
                            Some(Ok(None)) => Ok(()),
                            Some(Ok(Some(err)) | Err(err)) => Err(err),
                            None => Err(io::ErrorKind::NotConnected.into()),
                        });
                    }
                }
            }
            // The read side's.
            other => other.release(after),
        }
        after.wake(&mut self.waiters);
    }

    /// The operation to cancel for a lent buffer whose task went before
    /// any of it went out, while it is out with the driver: if the kernel
    /// has not sent from it yet, the write is withdrawn.
    pub(crate) fn withdrawal(&self) -> Option<u64> {
        self.lent
            .as_ref()
            .filter(|l| l.out && l.orphan && l.sent == 0)
            .and(self.out)
    }

    fn connected(&mut self, result: io::Result<()>) {
        self.connecting = false;
        self.connected = Some(result);
    }

    /// The lent buffer's operation completed: the buffer is the side's
    /// again, with how many of its bytes went. Of one whose task went, a
    /// write none of which went out is withdrawn.
    fn lent_back(&mut self, buf: Vec<u8>, answer: Answer<usize>, stale: bool, after: &mut After) {
        let Some(lent) = self.lent.as_mut().filter(|l| l.out && !stale) else {
            return;
        };
        lent.buf = buf;
        lent.out = false;
        match answer {
            Answer::Undone { not_ready } => self.not_ready(not_ready),
            Answer::Done(Ok(0)) => lent.done = Some(Err(io::ErrorKind::WriteZero.into())),
            Answer::Done(Ok(n)) => {
                lent.sent += n;
                if lent.sent >= lent.buf.len() {
                    lent.done = Some(Ok(lent.buf.len()));
                }
            }
            Answer::Done(Err(err)) => lent.done = Some(Err(err)),
        }
        let Some(lent) = self.lent.as_ref() else {
            return;
        };
        if lent.done.is_some() {
            self.lent_finished(after);
        } else if lent.orphan && lent.sent == 0 {
            let withdrawn: Option<Lent> = self.lent.take();
            drop(withdrawn);
        }
    }
}
