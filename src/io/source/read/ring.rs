//! The read side on a ring: the driver's part as operations of the source's
//! shard's ring, one in flight at a time, and their completions.
//!
//! What a ring operation brings goes where the driver's own calls put it on
//! epoll: a stream's bytes into the read-ahead (or into the lent buffer), a
//! listener's connection into the accepted ones, a datagram with its sender.
//! While one is out the side asks for nothing more; its completion clears
//! that, and wakes the tasks waiting, which take what came or ask again.
//! Only the operation's own completion clears it, so a side never has two
//! operations out, and its mailbox never more than one completion.

use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use super::{Ahead, Kind, ReadState};
use crate::io::side::After;
use crate::io::source::ring::{Done, Msg, OpMem, RawAddr, RingIo, RingOp};
use crate::syscore::linux::iouring::ring::Sqe;

impl ReadState {
    /// On the ring's thread: submits the read the waiting tasks want, if
    /// any wait.
    pub(crate) fn submit_wanted(
        &mut self,
        io: crate::io::source::Io<'_>,
        kind: Kind,
        after: &mut After,
    ) {
        if !self.waiters.is_empty() || self.lent_waits() {
            self.complete(io, kind, self.want, after);
        }
    }

    /// The driver's part on the ring: submits the read the waiting tasks
    /// need (`len` asked), unless one is out or a result waits to be taken.
    /// On another thread than the ring's, leaves the submission to it.
    pub(super) fn submit_ring(&mut self, r: RingIo<'_>, kind: Kind, len: usize, after: &mut After) {
        if self.ring_out.is_some() || self.has_result() || self.lent_done() {
            return;
        }
        let (ring, source, file) = match r {
            RingIo::Here { ring, source, file } => (ring, source, file),
            RingIo::Elsewhere { source } => {
                after.kick = source.home_held();
                return;
            }
            // Woken once the registration is in place.
            RingIo::NotYet => return,
        };
        let slot = file as RawFd;
        let (mem, sqe) = match kind {
            _ if self.poll_first => (OpMem::Poll, Sqe::poll_add(slot, libc::POLLIN as u32)),
            Kind::TcpListener | Kind::UnixListener => {
                let mut addr = RawAddr::empty();
                let sqe = Sqe::accept(
                    slot,
                    addr.as_mut_ptr(),
                    &mut addr.len,
                    libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                );
                (OpMem::Accept(addr), sqe)
            }
            Kind::Udp | Kind::UnixDatagram => {
                let Some(mut buf) = self.buffer(len, true, after) else {
                    return;
                };
                let spare = buf.spare();
                let mut msg = Msg::receiving(spare.as_mut_ptr(), spare.len());
                let sqe = Sqe::recvmsg(slot, &mut msg.hdr, 0);
                (OpMem::RecvMsg(msg, buf), sqe)
            }
            _ => match self.lent.as_mut().filter(|l| !l.out) {
                Some(lent) => {
                    let mut buf = core::mem::take(&mut lent.buf);
                    lent.out = true;
                    let spare = buf.spare_capacity_mut();
                    let sqe = stream_read(kind, slot, spare.as_mut_ptr().cast(), spare.len());
                    (OpMem::RecvLent(buf), sqe)
                }
                None => {
                    let Some(mut buf) = self.buffer(len, false, after) else {
                        return;
                    };
                    let spare = buf.spare();
                    let sqe = stream_read(kind, slot, spare.as_mut_ptr(), spare.len());
                    (OpMem::Recv(buf), sqe)
                }
            },
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

    /// The side's operation on the ring completed: what it brought goes
    /// where the tasks take it, and they are woken. One the kernel gave
    /// back undone brought nothing, and is asked again.
    pub(crate) fn ring_done(&mut self, done: Done, kind: Kind, after: &mut After) {
        self.ring_out = None;
        // An operation of a descriptor that is gone: what it holds is
        // dropped.
        let stale = done.epoch != self.epoch;
        let undone = done.undone();
        if !stale && done.not_ready() {
            self.poll_first = true;
        }
        let result = done.result();
        match done.mem {
            OpMem::Recv(buf) if stale || undone => after.drop_buf(buf),
            OpMem::Recv(mut buf) => match result {
                Ok(0) => {
                    self.eof = true;
                    after.drop_buf(buf);
                }
                Ok(n) => {
                    buf.set_filled(n);
                    self.ahead = Some(Ahead::Pool(buf));
                }
                Err(err) => {
                    self.error = Some(err);
                    after.drop_buf(buf);
                }
            },
            OpMem::RecvLent(mut buf) => {
                if let Some(lent) = self.lent.as_mut().filter(|l| l.out && !stale) {
                    if !undone {
                        if let Ok(n) = result {
                            // SAFETY: the kernel wrote the first `n` bytes of
                            // the buffer's spare capacity.
                            unsafe { buf.set_len(buf.len() + n) };
                        }
                        lent.done = Some(result);
                    }
                    lent.buf = buf;
                    lent.out = false;
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
            OpMem::Accept(_) if undone => {}
            OpMem::Accept(addr) => match result {
                Ok(fd) => {
                    // SAFETY: the kernel made this descriptor for us.
                    let conn = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
                    if !stale {
                        match addr.to_addr(kind) {
                            Ok(peer) => self.accepted.push_back((conn, peer)),
                            Err(err) => self.error = Some(err),
                        }
                    }
                }
                Err(_) if stale => {}
                Err(err) => self.error = Some(err),
            },
            OpMem::RecvMsg(_, buf) if stale || undone => after.drop_buf(buf),
            OpMem::RecvMsg(mut msg, mut buf) => match result {
                Ok(n) => match msg.sender(kind) {
                    Ok(from) => {
                        buf.set_filled(n);
                        self.datagrams.push_back((buf, from));
                    }
                    Err(err) => {
                        self.error = Some(err);
                        after.drop_buf(buf);
                    }
                },
                Err(err) => {
                    self.error = Some(err);
                    after.drop_buf(buf);
                }
            },
            OpMem::Poll if !stale && !undone => {
                self.poll_first = false;
                if let Err(err) = result {
                    self.error = Some(err);
                }
            }
            OpMem::Poll => {}
            // The write side's.
            _ => {}
        }
        // The tasks waiting take what came, or ask again.
        self.want = 0;
        after.wake(&mut self.waiters);
    }
}

/// A stream read of `len` bytes into `buf` on fixed slot `slot`: `recv` on
/// a socket, `read` at the current position on a pipe or a terminal.
fn stream_read(kind: Kind, slot: RawFd, buf: *mut u8, len: usize) -> Sqe {
    let len = u32::try_from(len).unwrap_or(u32::MAX);
    match kind {
        Kind::Bytes => Sqe::read(slot, buf, len, u64::MAX),
        _ => Sqe::recv(slot, buf, len, 0),
    }
}
