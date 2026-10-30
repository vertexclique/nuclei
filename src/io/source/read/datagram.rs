//! A datagram socket's receives that the task makes itself, on unix: on
//! epoll and kqueue every one, on a ring a Unix datagram's. The datagram
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
use crate::io::source::{Addr, Io, Kind, Via};
use crate::io::sys;

impl ReadState {
    /// The descriptor a datagram's task receives on itself: on epoll and
    /// kqueue every socket's, on a ring a Unix datagram socket's (a UDP
    /// datagram fits a buffer of the pool's largest class, which the ring
    /// receives into ahead). `None` leaves it to the driver.
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
            Via::Ops(_) if kind == Kind::UnixDatagram && !self.user_fd => fd,
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
            Via::Ops(_) => self.out.is_some() || self.poll_first,
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
    /// (proofs/tla/ring/RingRecv.tla). `None` leaves the receive to the
    /// driver.
    #[expect(
        clippy::too_many_arguments,
        reason = "the step's context, as every step's"
    )]
    pub(super) fn receive_now(
        &mut self,
        io: Io<'_>,
        fd: Option<RawFd>,
        kind: Kind,
        out: &mut [u8],
        peek: bool,
        waker: &Waker,
        after: &mut After,
    ) -> Option<Poll<io::Result<(usize, Addr)>>> {
        let fd = self.task_receives(io, fd, kind)?;
        if self.has_result() || self.lent.is_some() {
            return None;
        }
        if !self.waits_for_datagram(io) {
            match sys::ready(receive_into(fd, kind, out, peek)) {
                Some(result) => return Some(Poll::Ready(result)),
                None => {
                    if matches!(io.via, Via::Fd(_)) {
                        self.blocked = true;
                    }
                }
            }
        }
        self.wait_for_datagram(io, kind, waker, after);
        Some(Poll::Pending)
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
            match sys::ready(receive_into(fd, kind, &mut [0; 1], true)) {
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

/// Receives a datagram with its sender's address straight into `out`; with
/// `peek` it stays on the socket.
fn receive_into(fd: RawFd, kind: Kind, out: &mut [u8], peek: bool) -> io::Result<(usize, Addr)> {
    // SAFETY: the kernel only writes bytes into `out`, so it stays
    // initialized.
    let into = unsafe { &mut *(core::ptr::from_mut(out) as *mut [core::mem::MaybeUninit<u8>]) };
    match kind {
        Kind::UnixDatagram => sys::recv_from_unix_uninit(fd, into, peek)
            .map(|(n, a)| (n, Addr::Unix(crate::buf::spare::Spare::new(a)))),
        _ => sys::recv_from_udp_uninit(fd, into, peek).map(|(n, a)| (n, Addr::Inet(a))),
    }
}
