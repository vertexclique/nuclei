//! A datagram socket's sends, made by the task itself, so each send answers
//! what the kernel says of its own datagram, with no buffer of the pool
//! between (proofs/tla/ring/RingSend.tla): on unix, and on a completion port,
//! whose sockets are non-blocking for it. A ring source with no descriptor
//! copies and queues its datagram for the driver.

use core::task::{Poll, Waker};
use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;

use super::{Dest, WriteState};
use crate::io::side::After;
#[cfg(unix)]
use crate::io::source::Via;
use crate::io::source::{Io, Kind};
#[cfg(unix)]
use crate::io::sys;
#[cfg(unix)]
use crate::rt::counters::tally;

impl WriteState {
    /// A datagram handed to the kernel by the polling task once nothing of
    /// the side is ahead of it, so the call answers what the kernel says of
    /// this datagram, as std's does: a refusal included, which a datagram
    /// queued for the driver would only bring to the next call, and no
    /// buffer of the pool limits its size. With no room in the socket the
    /// call waits for an edge (epoll, kqueue) or a poll of the ring that says
    /// there is, and tries again (proofs/tla/ring/RingSend.tla).
    #[cfg(unix)]
    #[expect(
        clippy::too_many_arguments,
        reason = "the step's context, as every step's"
    )]
    pub(super) fn send_now(
        &mut self,
        io: Io<'_>,
        fd: RawFd,
        kind: Kind,
        bytes: &[u8],
        dest: Dest<'_>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        let ahead = match io.via {
            Via::Fd(_) => self.blocked,
            #[cfg(ring)]
            Via::Ops(_) => self.ring_holds() || self.poll_first,
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        };
        if !ahead {
            match sys::ready(super::send_datagram(fd, bytes, dest)).inspect(tally::writing) {
                Some(result) => return Poll::Ready(result),
                None => match io.via {
                    Via::Fd(_) => self.blocked = true,
                    #[cfg(ring)]
                    Via::Ops(_) => self.poll_first = true,
                    #[cfg(not(completion))]
                    Via::None(never, _) => match never {},
                },
            }
        }
        self.waiters.register(waker);
        self.push(io, kind, after);
        Poll::Pending
    }

    /// On a port, a datagram handed to the kernel by the polling task once
    /// nothing of the side is ahead of it, as on unix
    /// ([`send_now`](Self::send_now)): the call answers what the kernel says
    /// of this datagram, a refusal included, which a datagram queued for the
    /// port would only bring to the next call. With no room in the socket
    /// (`WSAEWOULDBLOCK`) the call waits for the port's poll of the socket for
    /// room (an AFD poll) and tries again (proofs/tla/ring/RingSend.tla,
    /// `Backend = "port"`).
    #[cfg(port)]
    #[expect(
        clippy::too_many_arguments,
        reason = "the step's context, as every step's"
    )]
    pub(super) fn send_now_port(
        &mut self,
        io: Io<'_>,
        fd: crate::io::raw::Raw,
        kind: Kind,
        bytes: &[u8],
        dest: Dest<'_>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if !self.ring_holds() && !self.poll_first {
            let to = match dest {
                Dest::Connected => None,
                Dest::Inet(addr) => Some(addr),
                Dest::Unix((never, _)) => match never {},
            };
            match crate::io::sys::send_datagram_now(fd, bytes, to.as_ref()) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => self.poll_first = true,
                sent => {
                    crate::rt::counters::tally::writing(&sent);
                    return Poll::Ready(sent);
                }
            }
        }
        self.waiters.register(waker);
        self.push(io, kind, after);
        Poll::Pending
    }

    /// A datagram copied and queued, and sent by the driver.
    #[cfg(completion)]
    pub(super) fn send_queued(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        bytes: &[u8],
        dest: Dest<'_>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        let Some(mut datagram) = self.buffer(io.pool(), bytes.len(), true, after) else {
            if let Some(err) = self.error.take() {
                return Poll::Ready(Err(err));
            }
            self.waiters.register(waker);
            return Poll::Pending;
        };
        datagram.fill_from(bytes);
        self.queued = Some((datagram, dest.to_owned()));
        self.push(io, kind, after);
        if self.queued.is_none()
            && let Some(err) = self.error.take()
        {
            // This datagram's own send failed.
            return Poll::Ready(Err(err));
        }
        if self.queued.is_some() {
            after.set_must_register();
        }
        Poll::Ready(Ok(bytes.len()))
    }
}
