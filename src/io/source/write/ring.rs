//! The write side on a ring: the driver's part as operations of the
//! source's shard's ring, one in flight at a time, and their completions.
//!
//! The operations go in the order the epoll driver makes its calls: a
//! connect, then the stream's bytes (what the ring gave back first, then
//! the write-behind, then the lent buffer), or a socket's datagrams in the
//! order they were queued. The write-behind moves into its operation, so
//! writes made while it is out start a new one behind it. The kernel may
//! take part of a stream's bytes: the rest goes out next, ahead of
//! anything written after. One the kernel gives back undone (cancelled as
//! the source moved, or with the socket not ready) took none of its bytes
//! and goes out again.

use std::io;
use std::os::fd::RawFd;

use super::{Kind, Lent, WriteState};
use crate::io::side::After;
use crate::io::source::ring::{Done, Msg, OpMem, RawAddr, RingIo, RingOp};
use crate::io::source::{Failure, Io, Target};
use crate::io::sys;
use crate::syscore::linux::iouring::ring::Sqe;

impl WriteState {
    /// What the side holds for the ring: a connect, bytes, a lent buffer
    /// not out yet, or a datagram.
    fn ring_work(&self) -> bool {
        self.dial.is_some()
            || self.again.is_some()
            || self.behind.is_some()
            || self
                .lent
                .as_ref()
                .is_some_and(|l| l.done.is_none() && !l.out)
            || self.queued.is_some()
    }

    /// Submits what the side holds, if this thread reaches a ring.
    pub(crate) fn submit_wanted(&mut self, io: Io<'_>, kind: Kind, after: &mut After) {
        if let Io::Ring(r) = io {
            self.submit_ring(r, kind, after);
        }
    }

    /// The driver's part on the ring: submits the side's next operation,
    /// unless one is out. On another thread than the ring's, leaves the
    /// submission to it.
    pub(super) fn submit_ring(&mut self, r: RingIo<'_>, kind: Kind, after: &mut After) {
        if self.ring_out.is_some() {
            return;
        }
        if let Some(failure) = self.failed
            && let Some(lent) = self.lent.as_mut().filter(|l| l.done.is_none() && !l.out)
        {
            // The write-behind before it failed: none of it goes out.
            lent.done = Some(Err(failure.error()));
            self.lent_finished(after);
        }
        if !self.poll_first && !self.ring_work() {
            return;
        }
        let (ring, source, file) = match r {
            RingIo::Here { ring, source, file } => (ring, source, file),
            RingIo::Elsewhere { source } => {
                after.kick = source.home_held();
                return;
            }
            // Submitted once the registration is in place.
            RingIo::NotYet => return,
        };
        let slot = file as RawFd;
        let (mem, sqe) = if self.poll_first {
            (OpMem::Poll, Sqe::poll_add(slot, libc::POLLOUT as u32))
        } else if let Some(addr) = self.dial.take() {
            let sqe = Sqe::connect(slot, addr.as_ptr(), addr.len);
            (OpMem::Connect(addr), sqe)
        } else if let Some(mem) = self
            .again
            .take()
            .or_else(|| self.behind.take().map(OpMem::Send))
        {
            let sqe = match &mem {
                OpMem::SendMsg(msg, _) => Sqe::sendmsg(slot, &msg.hdr, sys::SEND_FLAGS as u32),
                OpMem::Send(buf) => stream_write(kind, slot, buf.filled()),
                _ => return,
            };
            (mem, sqe)
        } else if let Some(lent) = self.lent.as_mut().filter(|l| l.done.is_none() && !l.out) {
            let buf = core::mem::take(&mut lent.buf);
            lent.out = true;
            let sqe = stream_write(kind, slot, buf.get(lent.sent..).unwrap_or_default());
            (OpMem::SendLent(buf), sqe)
        } else if let Some((buf, target)) = self.queued.take() {
            let to = match target {
                Target::Connected => None,
                Target::Inet(addr) => Some(RawAddr::inet(&addr)),
                Target::Unix(path) => match RawAddr::unix(&path) {
                    Ok(addr) => Some(addr),
                    Err(err) => {
                        self.error = Some(err);
                        after.drop_buf(buf);
                        return;
                    }
                },
            };
            let bytes = buf.filled();
            let msg = Msg::new(bytes.as_ptr().cast_mut(), bytes.len(), to);
            let sqe = Sqe::sendmsg(slot, &msg.hdr, sys::SEND_FLAGS as u32);
            (OpMem::SendMsg(msg, buf), sqe)
        } else {
            return;
        };
        self.ring_out = Some(ring.submit(
            RingOp {
                source: source.clone(),
                file,
                epoch: self.epoch,
                mem,
            },
            sqe,
        ));
    }

    /// The side's operation on the ring completed: the bytes the kernel
    /// took are gone, the rest goes out next, and the tasks waiting are
    /// woken. `fd` is the source's descriptor, for a connect's end.
    pub(crate) fn ring_done(&mut self, done: Done, fd: Option<RawFd>, after: &mut After) {
        self.ring_out = None;
        // An operation of a descriptor that is gone: what it holds is
        // dropped.
        let stale = done.epoch != self.epoch;
        let undone = done.undone();
        let connect = matches!(done.mem, OpMem::Connect(_));
        if !stale && !connect && done.not_ready() {
            self.poll_first = true;
        }
        let res = done.res;
        let result = done.result();
        match done.mem {
            mem @ (OpMem::Send(_) | OpMem::SendMsg(..)) if stale => mem.release(after),
            mem @ (OpMem::Send(_) | OpMem::SendMsg(..)) if undone => self.again = Some(mem),
            OpMem::Send(mut buf) => match result {
                Ok(n) if n > 0 => {
                    buf.consume(n);
                    if buf.is_empty() {
                        after.drop_buf(buf);
                    } else {
                        self.again = Some(OpMem::Send(buf));
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
            OpMem::SendMsg(_, buf) => {
                if let Err(err) = result {
                    self.error = Some(err);
                }
                after.drop_buf(buf);
            }
            OpMem::SendLent(buf) => self.lent_back(buf, result, stale || undone, stale, after),
            OpMem::Connect(_) if stale => {}
            // Cancelled as the source moved: asked again, the kernel says
            // whether it is under way (`EALREADY`) or ended.
            OpMem::Connect(addr) if res == -libc::ECANCELED => self.dial = Some(addr),
            OpMem::Connect(_) => match result {
                Err(err) if err.raw_os_error() == Some(libc::EALREADY) => self.poll_first = true,
                Err(err) if err.raw_os_error() == Some(libc::EISCONN) => self.connected(Ok(())),
                other => self.connected(other.map(drop)),
            },
            OpMem::Poll if !stale && !undone => {
                self.poll_first = false;
                if self.connecting {
                    // A connect under way ended: the socket says how.
                    self.connected(match fd.map(sys::take_error) {
                        Some(Ok(None)) => Ok(()),
                        Some(Ok(Some(err)) | Err(err)) => Err(err),
                        None => Err(io::ErrorKind::NotConnected.into()),
                    });
                }
            }
            OpMem::Poll => {}
            // The read side's.
            other => other.release(after),
        }
        after.wake(&mut self.waiters);
    }

    /// The operation to cancel for a lent buffer whose task went before
    /// any of it went out, while it is out with the ring: if the kernel has
    /// not sent from it yet, the write is withdrawn.
    pub(crate) fn withdrawal(&self) -> Option<u64> {
        self.lent
            .as_ref()
            .filter(|l| l.out && l.orphan && l.sent == 0)
            .and(self.ring_out)
    }

    fn connected(&mut self, result: io::Result<()>) {
        self.connecting = false;
        self.connected = Some(result);
    }

    /// The lent buffer's operation completed: the buffer is the side's
    /// again, with how many of its bytes went (none if `nothing`). Of one
    /// whose task went, a write none of which went out is withdrawn.
    fn lent_back(
        &mut self,
        buf: Vec<u8>,
        result: io::Result<usize>,
        nothing: bool,
        stale: bool,
        after: &mut After,
    ) {
        let Some(lent) = self.lent.as_mut().filter(|l| l.out && !stale) else {
            return;
        };
        lent.buf = buf;
        lent.out = false;
        if !nothing {
            match result {
                Ok(0) => lent.done = Some(Err(io::ErrorKind::WriteZero.into())),
                Ok(n) => {
                    lent.sent += n;
                    if lent.sent >= lent.buf.len() {
                        lent.done = Some(Ok(lent.buf.len()));
                    }
                }
                Err(err) => lent.done = Some(Err(err)),
            }
        }
        if lent.done.is_some() {
            self.lent_finished(after);
        } else if lent.orphan && lent.sent == 0 {
            let withdrawn: Option<Lent> = self.lent.take();
            drop(withdrawn);
        }
    }
}

/// A stream write of `bytes` on slot `slot`: `send` on a socket, `write` at
/// the current position on a pipe or a terminal.
fn stream_write(kind: Kind, slot: RawFd, bytes: &[u8]) -> Sqe {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    match kind {
        Kind::Bytes | Kind::File => Sqe::write(slot, bytes.as_ptr(), len, u64::MAX),
        _ => Sqe::send(slot, bytes.as_ptr(), len, sys::SEND_FLAGS),
    }
}
