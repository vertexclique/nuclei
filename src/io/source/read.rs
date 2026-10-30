//! The read side of a source: reads, peeks, accepts and datagram receives.
//!
//! Every call on the descriptor is the driver's ([`ReadState::complete`]),
//! into buffers of the pool: on an edge, for the tasks that wait, or at
//! once when a task asks and the socket may hold something. A task never
//! makes the call itself: it takes what the driver completed, copying it
//! into its own buffer. What no task took yet waits here: a stream's
//! read-ahead (in the order it came: a buffer of the pool, a lent buffer's
//! leftover, or on a ring the buffers of the shard's receive group that a
//! multishot receive handed out), its end or its error, a listener's
//! accepted connections, a datagram socket's datagrams (as many as tasks
//! wait, at most [`BATCH`]). A task whose future went away leaves it for the
//! next operation, so nothing the kernel handed over is lost.
//!
//! A stream read may also lend the side a buffer of its own
//! ([`Source::read_owned`]): the driver reads into that buffer, with no copy,
//! and the task takes it back with the bytes. One buffer is lent at a time;
//! while it waits for its bytes the driver reads into nothing else, and a
//! lent buffer whose task went away hands what the driver read into it to
//! the read-ahead, ahead of anything read after it.

use core::future::poll_fn;
use core::task::{Poll, Waker};
use std::collections::VecDeque;
use std::io;
#[cfg(readiness)]
use std::os::fd::RawFd;

use crate::io::raw::Owned;
use std::sync::Arc;
use std::task::Context;

use super::{Addr, Failure, Io, Kind, Source, Via};
use crate::buf::{PoolBuf, with_pool};
use crate::io::side::{After, Wakers};
#[cfg(readiness)]
use crate::io::sys;

mod ahead;
mod calls;
#[cfg(completion)]
mod ops;

use ahead::{Ahead, Piece};

/// The most connections or datagrams the driver takes ahead on one edge.
#[cfg(readiness)]
const BATCH: usize = 32;

/// A datagram received ahead: in a buffer of the pool, or on a ring in a
/// buffer of the receive group a multishot receive filled.
enum Datagram {
    Pool(PoolBuf),
    #[cfg(ring)]
    Ring(crate::rt::shard::recv::RingChunk),
}

impl Datagram {
    /// Copies the datagram into `out`, cut to it as the kernel cuts it.
    fn peek_into(&self, out: &mut [u8]) -> usize {
        match self {
            Datagram::Pool(buf) => buf.peek_into(out),
            #[cfg(ring)]
            Datagram::Ring(chunk) => {
                let src = chunk.bytes();
                let n = src.len().min(out.len());
                out[..n].copy_from_slice(&src[..n]);
                n
            }
        }
    }

    fn release(self, after: &mut After) {
        match self {
            Datagram::Pool(buf) => after.drop_buf(buf),
            // Goes back to its group as it drops.
            #[cfg(ring)]
            Datagram::Ring(_) => {}
        }
    }
}

/// The peer of a connection accepted ahead: its address, or, for one a
/// multishot accept brought, to be asked of the kernel if a task wants it.
enum Peer {
    Known(Addr),
    #[cfg(ring)]
    Ask,
}

/// A task's own buffer, lent to the side for the driver to read into.
struct Lent {
    buf: Vec<u8>,
    /// What the buffer held before: the read's bytes start here.
    from: usize,
    /// The read's result, once the driver made it.
    done: Option<io::Result<usize>>,
    /// The task to wake when it is made; `None` while the task polls.
    waker: Option<Waker>,
    /// The buffer is out with a ring operation (`buf` is empty meanwhile).
    out: bool,
    /// Its task went away while the buffer was out: the completion hands
    /// what came to the read-ahead.
    orphan: bool,
}

#[derive(Default)]
pub(super) struct ReadState {
    /// The last call would have blocked, and no edge came since.
    blocked: bool,
    waiters: Wakers,
    /// The pool had no buffer for the driver; the source waits for one.
    starved: bool,
    /// The largest read a waiting task asked for: what the driver reads.
    want: usize,
    ahead: Ahead,
    lent: Option<Lent>,
    eof: bool,
    error: Option<io::Error>,
    accepted: VecDeque<(Owned, Peer)>,
    datagrams: VecDeque<(Datagram, Addr)>,
    /// Urgent data or a hangup was reported: a short read may leave bytes
    /// behind it from now on (see [`drained`]).
    unsure: bool,
    /// The source's epoch this state is of.
    epoch: u32,
    /// Edges (and, on a ring, polls that completed) since the source was
    /// made: a caller's descriptor clears its ready only if none came after
    /// the look its guard was made from (proofs/tla/asyncfd).
    #[cfg(unix)]
    edges: u32,
    /// The side takes its caller's own calls (`AsyncFd`, a socket's
    /// `try_io` and `async_io`): the driver makes none, and says only when
    /// it may be ready.
    #[cfg(unix)]
    user_fd: bool,
    /// The operation out is the caller's poll.
    #[cfg(ring)]
    polling: bool,
    /// A receive of the driver's still out as the side turned to its
    /// caller's calls is being stopped.
    #[cfg(ring)]
    stopping: bool,
    /// The id of the operation out with the completion driver; only its
    /// completion clears this.
    #[cfg(completion)]
    out: Option<u64>,
    /// The ring gave an operation back because the socket was not ready:
    /// the next one waits for it to be.
    #[cfg(ring)]
    poll_first: bool,
    /// The operation out is a multishot receive, and whether it is being
    /// stopped (for a lent buffer, which takes the next bytes itself).
    #[cfg(ring)]
    multi: bool,
    #[cfg(ring)]
    multi_stopping: bool,
    /// The tasks that wait for connections take no address: an accept may
    /// go out as a multishot one.
    #[cfg(ring)]
    streams: bool,
    /// The receive group had no buffer left: the next receive takes one of
    /// the pool.
    #[cfg(ring)]
    dry: bool,
}

impl ReadState {
    pub(super) fn epoch_mut(&mut self) -> &mut u32 {
        &mut self.epoch
    }

    /// The id of the operation out with the ring, if one is.
    #[cfg(ring)]
    pub(super) fn out(&self) -> Option<u64> {
        self.out
    }

    pub(super) fn has_result(&self) -> bool {
        !self.ahead.is_empty()
            || self.eof
            || self.error.is_some()
            || !self.accepted.is_empty()
            || !self.datagrams.is_empty()
    }

    /// A lent buffer the driver has read into: nothing more is read until
    /// its task takes it back or goes away, so bytes stay in order.
    fn lent_done(&self) -> bool {
        self.lent.as_ref().is_some_and(|l| l.done.is_some())
    }

    /// A lent buffer waits for the driver.
    fn lent_waits(&self) -> bool {
        self.lent.as_ref().is_some_and(|l| l.done.is_none())
    }

    fn wait(&mut self, waker: &Waker, len: usize) {
        self.waiters.register(waker);
        self.want = self.want.max(len);
    }

    /// Whether a read, receive or accept would end now, taking nothing:
    /// what came ahead, the end, or an error, which stays for it. `Ready`
    /// with `true` at the end of the stream.
    pub(super) fn ready(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<bool> {
        // A datagram read ahead is read whole: the receive that takes it may
        // ask for any size, and one read into less would be cut. The pool's
        // largest buffer holds any datagram UDP carries; a pool too small
        // for one reads into what it has, as a receive would.
        let len = if kind.datagrams() {
            let largest = crate::buf::LARGEST;
            let room = with_pool(|pool| pool.is_some_and(|p| p.could_hold_whole(largest)));
            self.want.max(if room { largest } else { 1 })
        } else {
            self.want.max(1)
        };
        self.complete(io, kind, len, after);
        if self.has_result() {
            return Poll::Ready(self.eof);
        }
        self.wait(waker, len);
        Poll::Pending
    }

    /// The caller's own calls: `Ready` with the edge count while the side
    /// may be ready, else the task waits (on a ring, for a poll armed now).
    /// What the driver read for the side comes first: an error it met is
    /// reported, and data it holds fails the call, since a call of the
    /// caller's would take what comes after it; a receive still out on a
    /// ring is stopped and waited for.
    #[cfg(unix)]
    pub(super) fn fd_ready(
        &mut self,
        io: Io<'_>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<u32>> {
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        if !self.ahead.is_empty()
            || !self.accepted.is_empty()
            || !self.datagrams.is_empty()
            || self.lent.is_some()
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "the socket holds data it read ahead; read it before calls of your own",
            )));
        }
        self.user_fd = true;
        #[cfg(ring)]
        let driver_out = self.out.is_some() && !self.polling;
        #[cfg(not(ring))]
        let driver_out = false;
        if !self.blocked && !driver_out {
            return Poll::Ready(Ok(self.edges));
        }
        self.waiters.register(waker);
        self.complete(io, Kind::Fd, 0, after);
        Poll::Pending
    }

    /// The handle's own reads: the driver reads for the side again.
    #[cfg(unix)]
    pub(super) fn drive(&mut self) {
        self.user_fd = false;
    }

    /// The caller found its descriptor not ready: the side waits for the
    /// next edge, unless one came after the look `tick` is of.
    #[cfg(unix)]
    pub(super) fn fd_clear(&mut self, tick: u32) {
        if self.edges == tick {
            self.blocked = true;
        }
    }

    /// A buffer from this thread's pool, or the side starves until one
    /// comes back. A whole datagram no buffer of the pool could hold fails
    /// the operation instead.
    fn buffer(&mut self, len: usize, whole: bool, after: &mut After) -> Option<PoolBuf> {
        if whole && let Some(err) = crate::io::side::whole_too_large(None, len) {
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

    #[cfg(readiness)]
    /// The driver's part: on readiness, completes what the waiting tasks
    /// asked for, into the pool's buffers, and wakes them.
    pub(super) fn serve(&mut self, fd: RawFd, kind: Kind, edge: bool, after: &mut After) {
        if edge {
            self.blocked = false;
            self.edges = self.edges.wrapping_add(1);
            if self.user_fd {
                after.wake(&mut self.waiters);
                return;
            }
        }
        if self.waiters.is_empty() && !self.lent_waits() {
            return;
        }
        self.complete_fd(fd, kind, self.want, after);
        if self.has_result() {
            self.want = 0;
            after.wake(&mut self.waiters);
        }
    }

    /// The driver's call on the descriptor: a stream's bytes, into the lent
    /// buffer if there is one, else into the pool's (`len` asked); a
    /// listener's connections; a socket's datagrams (`len` each). Nothing
    /// while the last call would have blocked and no edge came since, or
    /// while a result waits to be taken.
    fn complete(&mut self, io: Io<'_>, kind: Kind, len: usize, after: &mut After) {
        match io.via {
            #[cfg(readiness)]
            Via::Fd(fd) => self.complete_fd(fd, kind, len, after),
            #[cfg(completion)]
            Via::Ops(route) => self.submit_ops(route, kind, len, after),
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        }
    }

    #[cfg(readiness)]
    /// The driver's call on the descriptor, as [`complete`](Self::complete).
    fn complete_fd(&mut self, fd: RawFd, kind: Kind, len: usize, after: &mut After) {
        if self.blocked || self.has_result() || self.lent_done() {
            return;
        }
        match kind {
            Kind::Stream | Kind::Tcp | Kind::Bytes | Kind::File => {
                if self.lent.is_some() {
                    self.read_lent(fd, kind, after);
                } else {
                    self.read_ahead(fd, kind, len, after);
                }
            }
            Kind::TcpListener | Kind::UnixListener => self.accept_ahead(fd, kind),
            Kind::Udp | Kind::UnixDatagram => self.receive_ahead(fd, kind, len, after),
            // The caller makes its own calls.
            Kind::Fd => {}
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
        copy: impl FnOnce(&mut Ahead, &mut After) -> usize,
    ) -> Option<io::Result<usize>> {
        if !self.ahead.is_empty() {
            return Some(Ok(copy(&mut self.ahead, after)));
        }
        if self.eof {
            // A read takes the end; a peek leaves it for the read.
            self.eof = peek;
            return Some(Ok(0));
        }
        self.error.take().map(Err)
    }

    #[cfg(readiness)]
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
                self.ahead.push(Piece::Pool(buf), after);
            }
            Some(Err(err)) => {
                self.error = Some(err);
                after.drop_buf(buf);
            }
        }
    }

    #[cfg(readiness)]
    /// Reads into the spare capacity of the lent buffer, and wakes its task.
    fn read_lent(&mut self, fd: RawFd, kind: Kind, after: &mut After) {
        let Some(lent) = self.lent.as_mut() else {
            return;
        };
        let spare = lent.buf.spare_capacity_mut();
        let asked = spare.len();
        let result = match sys::ready(sys::read_uninit(fd, spare)) {
            None => {
                self.blocked = true;
                return;
            }
            Some(result) => result,
        };
        if let Ok(n) = result {
            // SAFETY: the kernel wrote the first `n` bytes of the spare
            // capacity.
            unsafe { lent.buf.set_len(lent.buf.len() + n) };
            self.blocked = drained(kind, self.unsure, n, asked);
        }
        lent.done = Some(result);
        if let Some(waker) = lent.waker.take() {
            after.wake_one(waker);
        }
    }

    #[cfg(readiness)]
    fn accept_ahead(&mut self, fd: RawFd, kind: Kind) {
        let limit = self.waiters.len().clamp(1, BATCH);
        while self.accepted.len() < limit {
            match sys::ready(accept(fd, kind)) {
                None => {
                    self.blocked = true;
                    break;
                }
                Some(Ok((conn, peer))) => self.accepted.push_back((conn, Peer::Known(peer))),
                Some(Err(err)) => {
                    crate::rt::counters::accept_error(&err);
                    self.error = Some(err);
                    break;
                }
            }
        }
    }

    #[cfg(readiness)]
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
                    self.datagrams.push_back((Datagram::Pool(buf), addr));
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
        io: Io<'_>,
        kind: Kind,
        out: &mut [u8],
        peek: bool,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        self.complete(io, kind, out.len(), after);
        match self.take_stream(peek, after, |ahead, after| {
            ahead.copy_into(out, peek, after)
        }) {
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
        io: Io<'_>,
        kind: Kind,
        bufs: &mut [io::IoSliceMut<'_>],
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        let len = bufs.iter().map(|b| b.len()).sum();
        self.complete(io, kind, len, after);
        match self.take_stream(false, after, |ahead, after| {
            ahead.copy_vectored(bufs, after)
        }) {
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

    /// A stream read into a task's own buffer (`held`; `None` while it is
    /// lent): bytes another read left are moved into it; otherwise it is
    /// lent and the driver reads into it, at once if the socket may hold
    /// bytes and on the next edge if not. Ready with the buffer back in
    /// `held`.
    fn read_owned(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        held: &mut Option<Vec<u8>>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if let Some(buf) = held.as_mut() {
            if let Some(result) =
                self.take_stream(false, after, |ahead, after| ahead.append_to(buf, after))
            {
                self.pass_on(after);
                return Poll::Ready(result);
            }
            if self.lent.is_some() {
                // Another task's buffer is lent: this one waits its turn.
                self.wait(waker, 0);
                return Poll::Pending;
            }
            self.lent = held.take().map(|buf| Lent {
                from: buf.len(),
                buf,
                done: None,
                waker: None,
                out: false,
                orphan: false,
            });
        }
        // The task polls, so the driver's read needs no wake for it.
        let old = self.lent.as_mut().and_then(|l| l.waker.take());
        self.complete(io, kind, 0, after);
        let Some(lent) = self.lent.as_mut() else {
            return Poll::Ready(Err(io::Error::other("the read lost its buffer")));
        };
        if lent.done.is_none() {
            lent.waker = Some(match old {
                Some(w) if w.will_wake(waker) => w,
                other => {
                    if let Some(w) = other {
                        after.forget_one(w);
                    }
                    waker.clone()
                }
            });
            return Poll::Pending;
        }
        if let Some(w) = old {
            after.forget_one(w);
        }
        let Some(Lent { buf, done, .. }) = self.lent.take() else {
            return Poll::Ready(Err(io::Error::other("the read lost its buffer")));
        };
        *held = Some(buf);
        // Reads that waited for the lent buffer go on.
        after.wake(&mut self.waiters);
        Poll::Ready(done.unwrap_or(Ok(0)))
    }

    /// The task of the lent buffer went away: what the driver read into it
    /// becomes the read-ahead (its end or its error, the next read's), so
    /// nothing is lost.
    pub(super) fn abandon_lent(&mut self, after: &mut After) {
        if let Some(lent) = self.lent.as_mut().filter(|l| l.out) {
            // The buffer is out with a ring operation: its completion does
            // this.
            lent.orphan = true;
            if let Some(w) = lent.waker.take() {
                after.forget_one(w);
            }
            return;
        }
        let Some(lent) = self.lent.take() else {
            return;
        };
        if let Some(w) = lent.waker {
            after.forget_one(w);
        }
        match lent.done {
            None => {}
            Some(Ok(0)) => self.eof = true,
            Some(Ok(_)) => {
                debug_assert!(
                    self.ahead.is_empty(),
                    "bytes were read past the lent buffer"
                );
                self.ahead.push(
                    Piece::Left {
                        buf: lent.buf,
                        start: lent.from,
                    },
                    after,
                );
            }
            Some(Err(err)) => self.error = Some(err),
        }
        after.wake(&mut self.waiters);
    }

    /// The lent buffer back, as it is, for a poll that ends in an error.
    fn reclaim(&mut self, after: &mut After) -> Option<Vec<u8>> {
        if self.lent.as_ref().is_some_and(|l| l.out) {
            // Out with a ring operation, it cannot come back now: its
            // completion hands what came to the read-ahead.
            self.abandon_lent(after);
            return None;
        }
        let lent = self.lent.take()?;
        if let Some(w) = lent.waker {
            after.forget_one(w);
        }
        after.wake(&mut self.waiters);
        Some(lent.buf)
    }

    /// An accept: the driver accepts first if the listener may hold a
    /// connection, and the task takes it.
    fn accept(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<(Owned, Addr)>> {
        #[cfg(ring)]
        {
            self.streams = false;
        }
        self.complete(io, kind, 0, after);
        let taken = match self.accepted.pop_front() {
            Some((conn, Peer::Known(peer))) => Some(Ok((conn, peer))),
            #[cfg(ring)]
            Some((conn, Peer::Ask)) => {
                Some(super::ring::peer_of(&conn, kind).map(|peer| (conn, peer)))
            }
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

    /// An accept that takes no address, for a stream of connections: on a
    /// ring it may go out as a multishot accept.
    fn accept_stream(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<Owned>> {
        #[cfg(ring)]
        {
            self.streams = true;
        }
        self.complete(io, kind, 0, after);
        let taken = match self.accepted.pop_front() {
            Some((conn, _)) => Some(Ok(conn)),
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
        io: Io<'_>,
        kind: Kind,
        out: &mut [u8],
        peek: bool,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<(usize, Addr)>> {
        self.complete(io, kind, out.len(), after);
        let taken = match self.datagrams.front() {
            Some((datagram, addr)) => {
                let (n, addr) = (datagram.peek_into(out), addr.clone());
                if !peek && let Some((done, _)) = self.datagrams.pop_front() {
                    done.release(after);
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

    /// What the side holds, as a line, for a test that failed.
    #[cfg(test)]
    pub(super) fn report(&self) -> String {
        #[cfg(ring)]
        let ops = format!(
            ", out {:?}, multishot {}, stopping {}, poll first {}, dry {}, epoch {}",
            self.out, self.multi, self.multi_stopping, self.poll_first, self.dry, self.epoch
        );
        #[cfg(all(completion, not(ring)))]
        let ops = format!(", out {:?}, epoch {}", self.out, self.epoch);
        #[cfg(not(completion))]
        let ops = String::new();
        format!(
            "lent {:?}, read ahead {}, waiters {}, blocked {}, eof {}, error {:?}{ops}",
            self.lent.as_ref().map(|l| (
                l.buf.len(),
                l.from,
                l.done.as_ref().map(|d| d.as_ref().ok()),
                l.waker.is_some(),
                l.out
            )),
            self.ahead.len(),
            self.waiters.len(),
            self.blocked,
            self.eof,
            self.error.as_ref().map(io::Error::kind),
        )
    }

    /// Urgent data or a hangup was reported for the socket.
    pub(super) fn short_reads_unsure(&mut self) {
        self.unsure = true;
    }

    /// Registration failed: the waiting tasks get the error.
    pub(super) fn fail(&mut self, failure: Failure, after: &mut After) {
        self.error = Some(failure.error());
        if let Some(lent) = self.lent.as_mut().filter(|l| l.done.is_none()) {
            lent.done = Some(Err(failure.error()));
            if let Some(w) = lent.waker.take() {
                after.wake_one(w);
            }
        }
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
        if let Some(w) = self.lent.as_mut().and_then(|l| l.waker.take()) {
            after.wake_one(w);
        }
        after.wake(&mut self.waiters);
    }

    pub(super) fn forget_waiters(&mut self, after: &mut After) {
        self.starved = false;
        if let Some(w) = self.lent.as_mut().and_then(|l| l.waker.take()) {
            after.forget_one(w);
        }
        after.forget(&mut self.waiters);
    }

    /// An arrival of the multishot receive or accept, submitted in
    /// `epoch`: the tasks waiting take it. One of an older descriptor only
    /// goes.
    #[cfg(ring)]
    pub(super) fn take_arrival(
        &mut self,
        arrival: super::ring::Arrival,
        epoch: u32,
        after: &mut After,
    ) {
        if epoch != self.epoch {
            return;
        }
        match arrival {
            super::ring::Arrival::Chunk(chunk) => self.ahead.push(Piece::Ring(chunk), after),
            super::ring::Arrival::Conn(conn) => self.accepted.push_back((conn, Peer::Ask)),
            super::ring::Arrival::Datagram(payload, from) => {
                self.datagrams.push_back((Datagram::Ring(payload), from));
            }
        }
        self.want = 0;
        if let Some(w) = self.lent.as_mut().and_then(|l| l.waker.take()) {
            after.wake_one(w);
        }
        after.wake(&mut self.waiters);
    }

    /// Drops what the side holds: the descriptor it came from is gone.
    pub(super) fn discard(&mut self, after: &mut After) {
        self.ahead.release(after);
        if let Some(w) = self.lent.take().and_then(|l| l.waker) {
            after.forget_one(w);
        }
        for (datagram, _) in self.datagrams.drain(..) {
            datagram.release(after);
        }
        self.accepted.clear();
        self.eof = false;
        self.error = None;
        self.blocked = false;
        #[cfg(ring)]
        {
            self.poll_first = false;
        }
    }
}

#[cfg(readiness)]
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

#[cfg(readiness)]
/// The pollers that report urgent data.
const REPORTS_URGENT: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
));

#[cfg(readiness)]
/// Accepts a connection, non-blocking, with its peer's address.
fn accept(fd: RawFd, kind: Kind) -> io::Result<(Owned, Addr)> {
    match kind {
        Kind::UnixListener => {
            sys::accept_unix(fd).map(|(s, a)| (s, Addr::Unix(crate::buf::spare::Spare::new(a))))
        }
        _ => sys::accept_inet(fd).map(|(s, a)| (s, Addr::Inet(a))),
    }
}

#[cfg(readiness)]
/// Receives a datagram with its sender's address.
fn receive(fd: RawFd, kind: Kind, out: &mut [u8]) -> io::Result<(usize, Addr)> {
    match kind {
        Kind::UnixDatagram => sys::recv_from_unix(fd, out)
            .map(|(n, a)| (n, Addr::Unix(crate::buf::spare::Spare::new(a)))),
        _ => sys::recv_from_udp(fd, out).map(|(n, a)| (n, Addr::Inet(a))),
    }
}
