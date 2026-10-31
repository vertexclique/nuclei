//! A stream read straight into the caller's buffer: when it goes there
//! (`reads_itself`), the kernel's count of what a read leaves that lets it go
//! there at every size (`inq_wanted`), and the read itself (`read_direct`).

use core::task::{Poll, Waker};
use std::io;
use std::os::fd::RawFd;

#[cfg(any(target_os = "linux", target_os = "android"))]
use super::Inq;
use super::{Io, Kind, ReadState, Via, drained, sys};

impl ReadState {
    /// Whether a stream read (or `peek`) of `len` bytes goes straight into
    /// the task's buffer: a read big enough (see `direct`), or any while the
    /// pool has no buffer for the side, so a reader never waits on buffers
    /// that receives or reads ahead of other connections hold
    /// (proofs/tla/ring/RingStarve.tla, proofs/tla/starve/FdStarve.tla).
    #[cfg(unix)]
    #[inline(always)]
    pub(in crate::io::source) fn reads_itself(
        &self,
        io: Io<'_>,
        kind: Kind,
        len: usize,
        peek: bool,
    ) -> bool {
        if self.starved {
            return true;
        }
        !peek
            && match io.via {
                Via::Fd(_) => self.inq_wanted(kind) || super::super::direct::worth(io, len, false),
                _ => super::super::direct::worth(io, len, false),
            }
    }

    /// Whether a TCP or Unix stream read on a readiness driver goes straight
    /// into the task's buffer at every size: the kernel counts what a read
    /// leaves (`TCP_INQ`; a Unix stream's `SO_INQ`, Linux 6.17), so even a
    /// read that filled its buffer knows whether the socket is empty, and the
    /// next one waits for an edge instead of finding nothing
    /// (proofs/tla/starve/FdStarve.tla). A Unix stream's count leaves out a
    /// pending end, which its hangup edge reports instead, as for TCP. Not
    /// once the kernel refused the option, and not off Linux.
    #[cfg(unix)]
    pub(in crate::io::source) fn inq_wanted(&self, kind: Kind) -> bool {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return kind.counts_left() && self.inq != Inq::Off;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = kind;
            false
        }
    }

    /// Whether the receives of `fd` report what they leave: asks the kernel
    /// the first time.
    #[cfg(unix)]
    #[inline]
    pub(in crate::io::source) fn inq_on(&mut self, fd: RawFd, kind: Kind) -> bool {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            if self.inq == Inq::Unknown {
                self.inq = if sys::enable_inq(fd, kind == Kind::Tcp) {
                    Inq::On
                } else {
                    Inq::Off
                };
            }
            self.inq == Inq::On
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = (fd, kind);
            false
        }
    }

    /// Whether a read of `kind` may go straight into the task's buffer: a
    /// socket's, and while the pool has no buffer for the side any byte
    /// stream's (a pipe's, a terminal's).
    #[cfg(unix)]
    pub(super) fn reads_bytes(&self, kind: Kind) -> bool {
        kind.streams() || (self.starved && kind == Kind::Bytes)
    }

    /// A stream read (or `peek`) of `len` bytes made by `call` straight on
    /// the descriptor `fd`, into the caller's memory, if the side holds
    /// nothing that comes first. `None` leaves the read to the normal path,
    /// which waits for readiness when the socket had nothing; `Pending` is a
    /// multishot receive being stopped so the next poll can call.
    #[cfg(unix)]
    #[expect(
        clippy::too_many_arguments,
        reason = "the step's context, as every step's"
    )]
    #[inline(always)]
    pub(in crate::io::source) fn read_direct(
        &mut self,
        io: Io<'_>,
        fd: Option<RawFd>,
        kind: Kind,
        len: usize,
        peek: bool,
        waker: &Waker,
        call: impl FnOnce(RawFd, bool) -> io::Result<(usize, Option<usize>)>,
    ) -> Option<Poll<io::Result<usize>>> {
        let fd = fd?;
        if !self.reads_bytes(kind)
            || len == 0
            || !self.ahead.is_empty()
            || self.lent.is_some()
            || self.eof
            || self.error.is_some()
        {
            return None;
        }
        // `call` is told to count what the socket keeps, when it can.
        let inq =
            !peek && matches!(io.via, Via::Fd(_)) && self.inq_wanted(kind) && self.inq_on(fd, kind);
        if !inq && !self.reads_itself(io, kind, len, peek) {
            return None;
        }
        match io.via {
            Via::Fd(_) if self.blocked => return None,
            Via::Fd(_) => {}
            #[cfg(ring)]
            Via::Ops(route) => {
                if self.out.is_some() {
                    if !self.multi {
                        return None;
                    }
                    self.stop_multi(route);
                    self.wait(waker, len);
                    return Some(Poll::Pending);
                }
            }
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        }
        #[cfg(not(ring))]
        let _ = waker;
        #[cfg(ring)]
        {
            self.readable = false;
        }
        match sys::ready(call(fd, inq)) {
            None => {
                match io.via {
                    Via::Fd(_) => self.blocked = true,
                    #[cfg(ring)]
                    Via::Ops(_) => self.poll_first = true,
                    #[cfg(not(completion))]
                    Via::None(never, _) => match never {},
                }
                None
            }
            Some(Ok((n, left))) => {
                // A peek leaves the bytes in the socket, which no edge reports again.
                if matches!(io.via, Via::Fd(_)) && !peek {
                    self.blocked = match left {
                        // The kernel counted what is left: none, and nothing is.
                        Some(left) => n > 0 && !self.unsure && left == 0,
                        None => drained(kind, self.unsure, n, len),
                    };
                }
                Some(Poll::Ready(Ok(n)))
            }
            Some(Err(err)) => Some(Poll::Ready(Err(err))),
        }
    }
}
