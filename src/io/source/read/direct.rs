//! A stream read straight into the caller's buffer: when it goes there
//! (`reads_itself`), the kernel's count of what a read leaves that lets it go
//! there at every size (`inq_wanted`), and the read itself (`read_direct`).

use core::task::{Poll, Waker};
use std::io;
use std::os::fd::RawFd;

#[cfg(any(target_os = "linux", target_os = "android"))]
use super::Inq;
use super::{Io, Kind, ReadState, Via, drained, sys};
use crate::rt::counters::tally;

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

    /// Whether a poller's read of `len` bytes from `fd` asks the kernel to count what it leaves
    /// where the kernel can (the first), and does now (the second): not while the floor of its
    /// last count, less what was read since, covers more than this read, so the socket still
    /// holds bytes after it. One look on the way of a read with no floor kept.
    #[cfg(unix)]
    #[inline(always)]
    fn count_now(&mut self, fd: RawFd, kind: Kind, len: usize) -> (bool, bool) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        match self.inq {
            Inq::On => (true, true),
            Inq::Floor => (true, self.left as usize <= len),
            Inq::Unknown | Inq::Off => {
                let on = self.inq_wanted(kind) && self.inq_on(fd, kind);
                (on, on)
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = len;
            let on = self.inq_wanted(kind) && self.inq_on(fd, kind);
            (on, on)
        }
    }

    /// The kernel counted `left` bytes left after a read of `len`: kept as the floor when it
    /// covers more than another read of that size.
    #[cfg(unix)]
    #[inline(always)]
    fn keep_floor(&mut self, left: usize, len: usize) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if left > len {
            // A count past `u32` keeps what fits, still a floor.
            self.left = u32::try_from(left).unwrap_or(u32::MAX);
            self.inq = Inq::Floor;
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let _ = (left, len);
    }

    /// A read with no count took `n` bytes: the floor goes down by them.
    #[cfg(unix)]
    #[inline(always)]
    fn lower_floor(&mut self, n: usize) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            self.left = self
                .left
                .saturating_sub(u32::try_from(n).unwrap_or(u32::MAX));
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let _ = n;
    }

    /// The socket was found empty: no floor is left.
    #[cfg(unix)]
    #[inline(always)]
    fn forget_floor(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            self.left = 0;
        }
    }

    /// Whether the receives of `fd` report what they leave: asks the kernel
    /// the first time, out of line.
    #[cfg(unix)]
    #[inline(always)]
    pub(in crate::io::source) fn inq_on(&mut self, fd: RawFd, kind: Kind) -> bool {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            if self.inq == Inq::Unknown {
                self.ask_inq(fd, kind);
            }
            matches!(self.inq, Inq::On | Inq::Floor)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = (fd, kind);
            false
        }
    }

    /// [`inq_on`](Self::inq_on)'s first look: asks the kernel to count what
    /// the receives of `fd` leave.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[cold]
    #[inline(never)]
    fn ask_inq(&mut self, fd: RawFd, kind: Kind) {
        self.inq = if sys::enable_inq(fd, kind == Kind::Tcp) {
            Inq::On
        } else {
            Inq::Off
        };
    }

    /// Whether a read of `kind` may go straight into the task's buffer: a
    /// socket's, and while the pool has no buffer for the side any byte
    /// stream's (a pipe's, a terminal's).
    #[cfg(unix)]
    pub(super) fn reads_bytes(&self, kind: Kind) -> bool {
        kind.streams() || (self.starved && kind == Kind::Bytes)
    }

    /// A stream read (or `peek`) of `len` bytes made by `call` straight on
    /// the descriptor `fd`, into the caller's memory, if it goes there
    /// (`reads_itself`) and the side holds nothing that comes first. `None`
    /// leaves the read to the normal path, which waits for readiness when the
    /// socket had nothing; `Pending` is a multishot receive being stopped so
    /// the next poll can call.
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
        match io.via {
            // A poller's side that found the socket empty waits for the next edge.
            Via::Fd(_) if self.blocked => return None,
            Via::Fd(_) => {}
            // A small read stays with the ring's receive.
            _ if !self.reads_itself(io, kind, len, peek) => return None,
            _ => {}
        }
        if !self.reads_bytes(kind)
            || len == 0
            || !self.ahead.is_empty()
            || self.lent.is_some()
            || self.eof
            || self.error.is_some()
        {
            return None;
        }
        // `call` is told to count what the socket keeps, when it can and the floor of the
        // kernel's last count does not cover more than this read (`count_now`).
        let (inq, count) = if !peek && matches!(io.via, Via::Fd(_)) {
            self.count_now(fd, kind, len)
        } else {
            (false, false)
        };
        if !inq && matches!(io.via, Via::Fd(_)) && !self.reads_itself(io, kind, len, peek) {
            return None;
        }
        match io.via {
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
        // A read that came back goes on inline; one that failed is looked at out of line.
        let called = match call(fd, count) {
            Ok(read) => Some(Ok(read)),
            Err(err) => sys::ready(Err(err)),
        };
        match called {
            None => {
                match io.via {
                    Via::Fd(_) => {
                        self.blocked = true;
                        self.forget_floor();
                    }
                    #[cfg(ring)]
                    Via::Ops(_) => self.poll_first = true,
                    #[cfg(not(completion))]
                    Via::None(never, _) => match never {},
                }
                None
            }
            Some(Ok((n, left))) => {
                if !peek {
                    tally::read(n);
                }
                // A peek leaves the bytes in the socket, which no edge reports again.
                if matches!(io.via, Via::Fd(_)) && !peek {
                    self.blocked = match left {
                        // The kernel counted what is left: none, and nothing is.
                        Some(0) => n > 0 && !self.unsure,
                        Some(left) => {
                            self.keep_floor(left, len);
                            false
                        }
                        None => {
                            self.lower_floor(n);
                            drained(kind, self.unsure, n, len)
                        }
                    };
                }
                Some(Poll::Ready(Ok(n)))
            }
            Some(Err(err)) => Some(Poll::Ready(Err(err))),
        }
    }
}
