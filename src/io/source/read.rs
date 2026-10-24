//! The read side of a source: reads, peeks, accepts and datagram receives.
//!
//! Every call on the descriptor is the driver's ([`ReadState::complete`]),
//! into buffers of the pool: on an edge, for the tasks that wait, or at
//! once when a task asks and the socket may hold something. A task never
//! makes the call itself: it takes what the driver completed, copying it
//! into its own buffer. What no task took yet waits here: a stream's
//! read-ahead (one buffer at most), its end or its error, a listener's
//! accepted connections, a datagram socket's datagrams (as many as tasks
//! wait, at most [`BATCH`]). A task whose future went away leaves it for the
//! next operation, so nothing the kernel handed over is lost.

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
    /// Urgent data or a hangup was reported: a short read may leave bytes
    /// behind it from now on (see [`drained`]).
    unsure: bool,
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

    /// A buffer from this thread's pool, or the side starves until one
    /// comes back. A whole datagram no buffer of the pool could hold fails
    /// the operation instead.
    fn buffer(&mut self, len: usize, whole: bool, after: &mut After) -> Option<PoolBuf> {
        if whole && let Some(err) = crate::io::side::whole_too_large(len) {
            self.error = Some(err);
            return None;
        }
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
        if self.waiters.is_empty() {
            return;
        }
        self.complete(fd, kind, self.want, after);
        if self.has_result() {
            self.want = 0;
            after.wake(&mut self.waiters);
        }
    }

    /// The driver's call on the descriptor, into the pool's buffers: a
    /// stream's bytes (`len` asked), a listener's connections, a socket's
    /// datagrams (`len` each). Nothing while the last call would have
    /// blocked and no edge came since, or while a result waits to be taken.
    fn complete(&mut self, fd: RawFd, kind: Kind, len: usize, after: &mut After) {
        if self.blocked || self.has_result() {
            return;
        }
        match kind {
            Kind::Stream | Kind::Tcp | Kind::Bytes | Kind::File => {
                self.read_ahead(fd, kind, len, after)
            }
            Kind::TcpListener | Kind::UnixListener => self.accept_ahead(fd, kind),
            Kind::Udp | Kind::UnixDatagram => self.receive_ahead(fd, kind, len, after),
        }
    }

    /// A completed result for a task that asked: wakes the others waiting
    /// if some of it is left for them.
    fn pass_on(&mut self, after: &mut After) {
        if self.has_result() && !self.waiters.is_empty() {
            after.wake(&mut self.waiters);
        }
    }

    /// A stream's completed read, copied out by `copy` (which peeks when
    /// `peek`); `None` when nothing is completed.
    fn take_stream(
        &mut self,
        peek: bool,
        after: &mut After,
        copy: impl FnOnce(&mut PoolBuf) -> usize,
    ) -> Option<io::Result<usize>> {
        if let Some(ahead) = self.ahead.as_mut() {
            let n = copy(ahead);
            if ahead.is_empty()
                && let Some(done) = self.ahead.take()
            {
                after.drop_buf(done);
            }
            return Some(Ok(n));
        }
        if self.eof {
            // A read takes the end; a peek leaves it for the read.
            self.eof = peek;
            return Some(Ok(0));
        }
        self.error.take().map(Err)
    }

    fn read_ahead(&mut self, fd: RawFd, kind: Kind, len: usize, after: &mut After) {
        let Some(mut buf) = self.buffer(len, false, after) else {
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
                self.blocked = drained(kind, self.unsure, n, buf.capacity());
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

    fn receive_ahead(&mut self, fd: RawFd, kind: Kind, len: usize, after: &mut After) {
        let limit = self.waiters.len().clamp(1, BATCH);
        while self.datagrams.len() < limit {
            let Some(mut buf) = self.buffer(len, true, after) else {
                return;
            };
            match sys::ready(receive(fd, kind, buf.spare())) {
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

    /// A stream read (or peek) into `out`: the driver reads into the pool
    /// first if the socket may hold bytes, and the task copies them out.
    fn read(
        &mut self,
        fd: RawFd,
        kind: Kind,
        out: &mut [u8],
        peek: bool,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        self.complete(fd, kind, out.len(), after);
        let copy = |ahead: &mut PoolBuf| {
            if peek {
                ahead.peek_into(out)
            } else {
                ahead.read_into(out)
            }
        };
        match self.take_stream(peek, after, copy) {
            Some(result) => {
                self.pass_on(after);
                Poll::Ready(result)
            }
            None => {
                self.wait(waker, out.len());
                Poll::Pending
            }
        }
    }

    /// A stream read scattered into `bufs`, as [`read`](Self::read).
    fn read_vectored(
        &mut self,
        fd: RawFd,
        kind: Kind,
        bufs: &mut [io::IoSliceMut<'_>],
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        let len = bufs.iter().map(|b| b.len()).sum();
        self.complete(fd, kind, len, after);
        match self.take_stream(false, after, |ahead| ahead.read_into_vectored(bufs)) {
            Some(result) => {
                self.pass_on(after);
                Poll::Ready(result)
            }
            None => {
                self.wait(waker, len);
                Poll::Pending
            }
        }
    }

    /// An accept: the driver accepts first if the listener may hold a
    /// connection, and the task takes it.
    fn accept(
        &mut self,
        fd: RawFd,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<(OwnedFd, Addr)>> {
        self.complete(fd, kind, 0, after);
        let taken = match self.accepted.pop_front() {
            Some(conn) => Some(Ok(conn)),
            None => self.error.take().map(Err),
        };
        match taken {
            Some(result) => {
                self.pass_on(after);
                Poll::Ready(result)
            }
            None => {
                self.wait(waker, 0);
                Poll::Pending
            }
        }
    }

    /// A datagram receive (or peek) into `out`: the driver receives into the
    /// pool first if the socket may hold one, and the task copies it out
    /// (what does not fit `out` is cut, as the kernel cuts it).
    fn receive(
        &mut self,
        fd: RawFd,
        kind: Kind,
        out: &mut [u8],
        peek: bool,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<(usize, Addr)>> {
        self.complete(fd, kind, out.len(), after);
        let taken = match self.datagrams.front() {
            Some((datagram, addr)) => {
                let (n, addr) = (datagram.peek_into(out), addr.clone());
                if !peek && let Some((done, _)) = self.datagrams.pop_front() {
                    after.drop_buf(done);
                }
                Some(Ok((n, addr)))
            }
            None => self.error.take().map(Err),
        };
        match taken {
            Some(result) => {
                self.pass_on(after);
                Poll::Ready(result)
            }
            None => {
                self.wait(waker, out.len());
                Poll::Pending
            }
        }
    }

    /// Urgent data or a hangup was reported for the socket.
    pub(super) fn short_reads_unsure(&mut self) {
        self.unsure = true;
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

/// Whether a TCP read of `n` bytes, when `asked` were asked for, emptied the
/// socket, so the next read can wait for the next edge instead of making a
/// call that would find nothing.
///
/// TCP hands a read all its receive queue and backlog hold, up to what was
/// asked; it stops short of that with bytes still queued only at urgent
/// data, or with a hangup (a FIN or an error) behind the bytes. Both raise
/// an event (`EPOLLPRI` or Apple's `EV_OOBAND`; a hangup event) that marks
/// the socket `unsure` before the driver reads, and an unsure socket never
/// takes a short read as draining again. Bytes that come later raise a new
/// edge: the edge-triggered events fire on each arrival. A Unix stream may
/// stop between two writers or at passed descriptors with nothing to tell,
/// a pipe's wakes are not promised per write, a datagram socket may hold
/// more datagrams, and a kqueue that does not report urgent data cannot say
/// when to stop trusting it: none of them is taken as drained.
fn drained(kind: Kind, unsure: bool, n: usize, asked: usize) -> bool {
    REPORTS_URGENT && kind == Kind::Tcp && !unsure && n > 0 && n < asked
}

/// The pollers that report urgent data.
const REPORTS_URGENT: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
));

/// Accepts a connection, non-blocking, with its peer's address.
fn accept(fd: RawFd, kind: Kind) -> io::Result<(OwnedFd, Addr)> {
    match kind {
        Kind::UnixListener => sys::accept_unix(fd).map(|(s, a)| (s, Addr::Unix(a))),
        _ => sys::accept_inet(fd).map(|(s, a)| (s, Addr::Inet(a))),
    }
}

/// Receives a datagram with its sender's address.
fn receive(fd: RawFd, kind: Kind, out: &mut [u8]) -> io::Result<(usize, Addr)> {
    match kind {
        Kind::UnixDatagram => sys::recv_from_unix(fd, out).map(|(n, a)| (n, Addr::Unix(a))),
        _ => sys::recv_from_udp(fd, out).map(|(n, a)| (n, Addr::Inet(a))),
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
            |st, after| st.read(fd, kind, out, peek, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
    }

    /// Reads from a stream, scattered into `bufs` in order.
    pub(crate) fn poll_read_vectored(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        if bufs.iter().all(|b| b.is_empty()) {
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
            |st, after| st.read_vectored(fd, kind, bufs, cx.waker(), after),
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
            |st, after| st.accept(fd, kind, cx.waker(), after),
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
