//! The read side of a source: reads, peeks, accepts and datagram receives.
//!
//! What the driver completed and no task took yet waits here: a stream's
//! read-ahead (one buffer at most), its end or its error, a listener's
//! accepted connections, a datagram socket's datagrams (as many as tasks
//! wait, at most [`BATCH`]). A task takes it on its next poll; a task whose
//! future went away leaves it for the next operation, so nothing the kernel
//! handed over is lost.

use core::task::{Poll, Waker};
use std::collections::VecDeque;
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::sync::Arc;
use std::task::Context;

use super::{Addr, Failure, Kind, Source};
use crate::buf::{PoolBuf, with_pool};
use crate::io::side::{After, Wakers};
use crate::io::sys;

/// The most connections or datagrams the driver takes ahead on one edge.
const BATCH: usize = 32;

#[derive(Default)]
pub(super) struct ReadState {
    /// The last call would have blocked, and no edge came since.
    blocked: bool,
    waiters: Wakers,
    /// The pool had no buffer for the driver; the source waits for one.
    starved: bool,
    /// The largest read a waiting task asked for: what the driver reads.
    want: usize,
    ahead: Option<PoolBuf>,
    eof: bool,
    error: Option<io::Error>,
    accepted: VecDeque<(OwnedFd, Addr)>,
    datagrams: VecDeque<(PoolBuf, Addr)>,
}

impl ReadState {
    pub(super) fn has_result(&self) -> bool {
        self.ahead.is_some()
            || self.eof
            || self.error.is_some()
            || !self.accepted.is_empty()
            || !self.datagrams.is_empty()
    }

    fn wait(&mut self, waker: &Waker, len: usize) {
        self.waiters.register(waker);
        self.want = self.want.max(len);
    }

    /// A buffer from this thread's pool, or the side starves.
    fn buffer(&mut self, len: usize, whole: bool, after: &mut After) -> Option<PoolBuf> {
        let buf =
            with_pool(|pool| pool.and_then(|p| if whole { p.get_whole(len) } else { p.get(len) }));
        if buf.is_none() && !self.starved {
            self.starved = true;
            after.starved = true;
        }
        buf
    }

    /// The driver's part: on readiness, completes what the waiting tasks
    /// asked for, into the pool's buffers, and wakes them.
    pub(super) fn serve(&mut self, fd: RawFd, kind: Kind, edge: bool, after: &mut After) {
        if edge {
            self.blocked = false;
        }
        if self.blocked || self.waiters.is_empty() {
            return;
        }
        if !self.has_result() {
            match kind {
                Kind::Stream | Kind::Bytes | Kind::File => self.read_ahead(fd, after),
                Kind::TcpListener | Kind::UnixListener => self.accept_ahead(fd, kind),
                Kind::Udp | Kind::UnixDatagram => self.receive_ahead(fd, kind, after),
            }
        }
        if self.has_result() {
            self.want = 0;
            after.wake(&mut self.waiters);
        }
    }

    fn read_ahead(&mut self, fd: RawFd, after: &mut After) {
        let Some(mut buf) = self.buffer(self.want, false, after) else {
            return;
        };
        match sys::ready(sys::read(fd, buf.spare())) {
            None => {
                self.blocked = true;
                after.drop_buf(buf);
            }
            Some(Ok(0)) => {
                self.eof = true;
                after.drop_buf(buf);
            }
            Some(Ok(n)) => {
                buf.set_filled(n);
                self.ahead = Some(buf);
            }
            Some(Err(err)) => {
                self.error = Some(err);
                after.drop_buf(buf);
            }
        }
    }

    fn accept_ahead(&mut self, fd: RawFd, kind: Kind) {
        let limit = self.waiters.len().clamp(1, BATCH);
        while self.accepted.len() < limit {
            match sys::ready(accept(fd, kind)) {
                None => {
                    self.blocked = true;
                    break;
                }
                Some(Ok(conn)) => self.accepted.push_back(conn),
                Some(Err(err)) => {
                    self.error = Some(err);
                    break;
                }
            }
        }
    }

    fn receive_ahead(&mut self, fd: RawFd, kind: Kind, after: &mut After) {
        let limit = self.waiters.len().clamp(1, BATCH);
        while self.datagrams.len() < limit {
            let Some(mut buf) = self.buffer(self.want, true, after) else {
                return;
            };
            match sys::ready(receive(fd, kind, buf.spare(), false)) {
                None => {
                    self.blocked = true;
                    after.drop_buf(buf);
                    return;
                }
                Some(Ok((n, addr))) => {
                    buf.set_filled(n);
                    self.datagrams.push_back((buf, addr));
                }
                Some(Err(err)) => {
                    self.error = Some(err);
                    after.drop_buf(buf);
                    return;
                }
            }
        }
    }

    /// A stream read (or peek) into `out`.
    fn read(
        &mut self,
        fd: RawFd,
        out: &mut [u8],
        peek: bool,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if let Some(ahead) = self.ahead.as_mut() {
            let n = if peek {
                ahead.peek_into(out)
            } else {
                ahead.read_into(out)
            };
            if ahead.is_empty()
                && let Some(done) = self.ahead.take()
            {
                after.drop_buf(done);
            }
            return Poll::Ready(Ok(n));
        }
        if self.eof {
            // A read takes the end; a peek leaves it for the read.
            self.eof = peek;
            return Poll::Ready(Ok(0));
        }
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        if !self.blocked {
            let result = if peek {
                sys::recv(fd, out, libc::MSG_PEEK)
            } else {
                sys::read(fd, out)
            };
            match sys::ready(result) {
                Some(result) => return Poll::Ready(result),
                None => self.blocked = true,
            }
        }
        self.wait(waker, out.len());
        Poll::Pending
    }

    fn accept(
        &mut self,
        fd: RawFd,
        kind: Kind,
        waker: &Waker,
    ) -> Poll<io::Result<(OwnedFd, Addr)>> {
        if let Some(conn) = self.accepted.pop_front() {
            return Poll::Ready(Ok(conn));
        }
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        if !self.blocked {
            match sys::ready(accept(fd, kind)) {
                Some(result) => return Poll::Ready(result),
                None => self.blocked = true,
            }
        }
        self.wait(waker, 0);
        Poll::Pending
    }

    fn receive(
        &mut self,
        fd: RawFd,
        kind: Kind,
        out: &mut [u8],
        peek: bool,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<(usize, Addr)>> {
        if let Some((datagram, addr)) = self.datagrams.front() {
            // What does not fit `out` is cut, as the kernel cuts it.
            let n = datagram.peek_into(out);
            let addr = addr.clone();
            if !peek && let Some((done, _)) = self.datagrams.pop_front() {
                after.drop_buf(done);
            }
            return Poll::Ready(Ok((n, addr)));
        }
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        if !self.blocked {
            match sys::ready(receive(fd, kind, out, peek)) {
                Some(result) => return Poll::Ready(result),
                None => self.blocked = true,
            }
        }
        self.wait(waker, out.len());
        Poll::Pending
    }

    /// Registration failed: the waiting tasks get the error.
    pub(super) fn fail(&mut self, failure: Failure, after: &mut After) {
        self.error = Some(failure.error());
        after.wake(&mut self.waiters);
    }

    /// A buffer came back to the pool this side ran out of.
    pub(super) fn unstarve(&mut self, after: &mut After) {
        if self.starved {
            self.starved = false;
            after.wake(&mut self.waiters);
        }
    }

    /// Every waiting task looks again.
    pub(super) fn wake_all(&mut self, after: &mut After) {
        self.starved = false;
        after.wake(&mut self.waiters);
    }

    pub(super) fn forget_waiters(&mut self, after: &mut After) {
        self.starved = false;
        after.forget(&mut self.waiters);
    }

    /// Drops what the side holds: the descriptor it came from is gone.
    pub(super) fn discard(&mut self, after: &mut After) {
        if let Some(buf) = self.ahead.take() {
            after.drop_buf(buf);
        }
        for (buf, _) in self.datagrams.drain(..) {
            after.drop_buf(buf);
        }
        self.accepted.clear();
        self.eof = false;
        self.error = None;
        self.blocked = false;
    }
}

/// Accepts a connection, non-blocking, with its peer's address.
fn accept(fd: RawFd, kind: Kind) -> io::Result<(OwnedFd, Addr)> {
    match kind {
        Kind::UnixListener => sys::accept_unix(fd).map(|(s, a)| (s, Addr::Unix(a))),
        _ => sys::accept_inet(fd).map(|(s, a)| (s, Addr::Inet(a))),
    }
}

/// Receives a datagram with its sender's address.
fn receive(fd: RawFd, kind: Kind, out: &mut [u8], peek: bool) -> io::Result<(usize, Addr)> {
    match kind {
        Kind::UnixDatagram => sys::recv_from_unix(fd, out, peek).map(|(n, a)| (n, Addr::Unix(a))),
        _ => sys::recv_from_udp(fd, out, peek).map(|(n, a)| (n, Addr::Inet(a))),
    }
}

impl Source {
    /// Reads from a stream into `out`; `peek` leaves the bytes to be read
    /// again.
    pub(crate) fn poll_read(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<usize>> {
        if out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.read.poll(
            &mut after,
            |st, after| st.read(fd, out, peek, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
    }

    /// Accepts a connection from a listener.
    pub(crate) fn poll_accept(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(OwnedFd, Addr)>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.read.poll(
            &mut after,
            |st, _| st.accept(fd, kind, cx.waker()),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
    }

    /// Receives a datagram into `out`, with its sender; `peek` leaves it to
    /// be received again.
    pub(crate) fn poll_receive(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<(usize, Addr)>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.read.poll(
            &mut after,
            |st, after| st.receive(fd, kind, out, peek, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
    }
}
