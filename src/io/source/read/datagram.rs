//! A datagram socket's receives that the task makes itself, on unix: on
//! epoll and kqueue every one, on a ring a Unix datagram's, and a UDP one's
//! while the pool has no buffer for the side. The datagram
//! goes from the kernel straight into the task's buffer, whole whenever the
//! buffer holds it, so no buffer of the pool cuts it or refuses it for its
//! size; a look at readability looks and takes nothing. With no datagram
//! there the task waits for an edge or the ring's poll, and tries again
//! (proofs/tla/ring/RingRecv.tla).

use core::task::{Poll, Waker};
use std::io;
use std::os::fd::RawFd;

use super::ReadState;
use crate::io::side::After;
use crate::io::source::{Addr, Io, Kind, Sender, Via, quick};
use crate::io::sys;
use crate::rt::counters::tally;

impl ReadState {
    /// The descriptor a datagram's task receives on itself: on epoll and
    /// kqueue every socket's, on a ring a Unix datagram socket's (a UDP
    /// datagram fits a buffer of the pool's largest class, which the ring
    /// receives into ahead, unless the pool has none for the side: then the
    /// task receives too, proofs/tla/ring/RingStarve.tla). `None` leaves it
    /// to the driver.
    #[cfg_attr(
        not(ring),
        expect(
            unused_variables,
            reason = "only a ring reads the descriptor and the kind"
        )
    )]
    pub(super) fn task_receives(&self, io: Io<'_>, fd: Option<RawFd>, kind: Kind) -> Option<RawFd> {
        match io.via {
            #[cfg(readiness)]
            Via::Fd(fd) => Some(fd),
            #[cfg(ring)]
            Via::Ops(_) if (kind == Kind::UnixDatagram || self.starved) && !self.user_fd => fd,
            #[cfg(ring)]
            Via::Ops(_) => None,
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        }
    }

    /// Whether a task that receives a datagram itself waits: for an edge, or
    /// behind the ring's operation or poll.
    fn waits_for_datagram(&self, io: Io<'_>) -> bool {
        match io.via {
            #[cfg(readiness)]
            Via::Fd(_) => self.blocked,
            #[cfg(ring)]
            Via::Ops(_) => self.poll_first || (self.out.is_some() && !self.armed),
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        }
    }

    /// A task that receives a datagram itself waits for one: for the next
    /// edge, or for the ring's poll, whose end wakes it.
    fn wait_for_datagram(&mut self, io: Io<'_>, kind: Kind, waker: &Waker, after: &mut After) {
        match io.via {
            #[cfg(readiness)]
            Via::Fd(_) => {}
            #[cfg(ring)]
            Via::Ops(_) => {
                self.poll_first = true;
                self.task_polls = true;
            }
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        }
        self.wait(waker, 0);
        self.complete(io, kind, 0, after);
    }

    /// A datagram received straight into `out` by the polling task (left on
    /// the socket with `peek`), so it comes whole whenever `out` holds it,
    /// whatever the pool may hold, and cut to `out` as the kernel cuts it.
    /// Nothing the side holds may come first. With none there the call waits
    /// for an edge or the ring's poll, and tries again
    /// (proofs/tla/ring/RingRecv.tla). `seen` is the word a call made with no
    /// guard saw before the kernel answered it "not now": if no edge was
    /// counted since, the socket had nothing after the last edge, and the
    /// call waits without asking again (proofs/tla/dgram). `None` leaves the
    /// receive to the driver.
    #[expect(
        clippy::too_many_arguments,
        reason = "the step's context, as every step's"
    )]
    #[inline(always)]
    pub(crate) fn receive_now<A: Sender>(
        &mut self,
        io: Io<'_>,
        fd: Option<RawFd>,
        kind: Kind,
        out: &mut [u8],
        peek: bool,
        seen: Option<u32>,
        waker: &Waker,
        after: &mut After,
    ) -> Option<Poll<io::Result<(usize, A)>>> {
        let fd = self.task_receives(io, fd, kind)?;
        if self.has_result() || self.lent.is_some() {
            return None;
        }
        if seen == Some(quick::read_word(self.edges, false)) {
            self.came_empty(io);
        }
        if !self.waits_for_datagram(io) {
            #[cfg(ring)]
            {
                self.readable = false;
            }
            match sys::ready(A::receive(fd, kind, out, peek)) {
                Some(result) => {
                    if !peek {
                        tally::received(&result);
                    }
                    return Some(Poll::Ready(result));
                }
                None => self.came_empty(io),
            }
        }
        self.wait_for_datagram(io, kind, waker, after);
        Some(Poll::Pending)
    }

    /// The word a datagram receive made with no guard reads (`quick`): the
    /// edges counted, and whether the receive must go through the side,
    /// which it must while the side holds what comes first or a lent
    /// buffer, the caller makes its own calls, the last call found the
    /// socket empty and no edge came, an operation of the ring is out, or
    /// the ring receives the side's datagrams ahead.
    #[cfg_attr(
        not(ring),
        expect(
            unused_variables,
            reason = "only a ring receives ahead or has operations out"
        )
    )]
    #[inline(always)]
    pub(crate) fn quick_word(&self, kind: Kind, on_ring: bool) -> u32 {
        let holds = self.has_result() || self.lent.is_some() || self.user_fd || self.blocked;
        #[cfg(ring)]
        let holds = holds
            || (self.out.is_some() && !self.armed)
            || self.poll_first
            || (on_ring && kind != Kind::UnixDatagram && !self.starved);
        quick::read_word(self.edges, holds)
    }

    /// The kernel had no datagram for the task's own receive: the next one
    /// waits for an edge, or behind the ring's poll.
    fn came_empty(&mut self, io: Io<'_>) {
        match io.via {
            #[cfg(readiness)]
            Via::Fd(_) => self.blocked = true,
            #[cfg(ring)]
            Via::Ops(_) => {
                self.readable = false;
                self.poll_first = true;
            }
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        }
    }

    /// Whether a datagram is there for a task that receives it itself:
    /// looked at and left, so none is read into a buffer smaller than it.
    pub(super) fn datagram_ready(
        &mut self,
        io: Io<'_>,
        fd: RawFd,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<bool> {
        if self.has_result() || self.lent_done() {
            return Poll::Ready(self.eof);
        }
        if !self.waits_for_datagram(io) {
            #[cfg(ring)]
            {
                self.readable = false;
            }
            match sys::ready(Addr::receive(fd, kind, &mut [0; 1], true)) {
                Some(Ok(_)) => return Poll::Ready(false),
                Some(Err(err)) => {
                    self.error = Some(err);
                    return Poll::Ready(false);
                }
                None => {
                    if matches!(io.via, Via::Fd(_)) {
                        self.blocked = true;
                    }
                }
            }
        }
        self.wait_for_datagram(io, kind, waker, after);
        Poll::Pending
    }
}
