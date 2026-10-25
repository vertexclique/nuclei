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

use super::{Addr, Failure, Io, Kind, Source};
use crate::buf::{PoolBuf, with_pool};
use crate::io::side::{After, Wakers};
#[cfg(readiness)]
use crate::io::sys;

#[cfg(completion)]
mod ops;

/// The most connections or datagrams the driver takes ahead on one edge.
#[cfg(readiness)]
const BATCH: usize = 32;

/// One run of bytes read ahead.
enum Piece {
    /// Read by the driver into a buffer of the pool.
    Pool(PoolBuf),
    /// Read into a lent buffer, from `start`, whose task went away.
    // vertexia: the caller's allocation, kept uncharged by the pool (one per
    // source at most, freed by the next reads); copy it into pool buffers if
    // kept buffers must count against the cap.
    Left { buf: Vec<u8>, start: usize },
    /// A buffer of the shard's receive group a multishot receive filled;
    /// it goes back to the group once taken.
    #[cfg(ring)]
    Ring(crate::rt::shard::recv::RingChunk),
}

impl Piece {
    fn filled(&self) -> &[u8] {
        match self {
            Piece::Pool(buf) => buf.filled(),
            Piece::Left { buf, start } => buf.get(*start..).unwrap_or_default(),
            #[cfg(ring)]
            Piece::Ring(chunk) => chunk.bytes(),
        }
    }

    fn consume(&mut self, n: usize) {
        match self {
            Piece::Pool(buf) => buf.consume(n),
            Piece::Left { buf, start } => *start = (*start + n).min(buf.len()),
            #[cfg(ring)]
            Piece::Ring(chunk) => chunk.consume(n),
        }
    }

    fn release(self, after: &mut After) {
        if let Piece::Pool(buf) = self {
            after.drop_buf(buf);
        }
    }
}

/// Bytes read ahead that no task took yet, in the order they came: what
/// the driver read into the pool, what a lent buffer whose task went away
/// holds, what multishot receives handed out.
#[derive(Default)]
struct Ahead {
    pieces: VecDeque<Piece>,
}

impl Ahead {
    fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// The bytes held.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.pieces.iter().map(|p| p.filled().len()).sum()
    }

    /// Adds bytes that came after those held.
    fn push(&mut self, piece: Piece, after: &mut After) {
        if piece.filled().is_empty() {
            piece.release(after);
        } else {
            self.pieces.push_back(piece);
        }
    }

    /// Takes `n` bytes off the front, letting go of the pieces emptied.
    fn consume(&mut self, mut n: usize, after: &mut After) {
        while n > 0 {
            let Some(front) = self.pieces.front_mut() else {
                return;
            };
            let here = front.filled().len().min(n);
            front.consume(here);
            n -= here;
            if front.filled().is_empty()
                && let Some(done) = self.pieces.pop_front()
            {
                done.release(after);
            }
        }
    }

    /// Copies bytes into `out`, in order, consumed unless `peek`; returns
    /// how many.
    fn copy_into(&mut self, out: &mut [u8], peek: bool, after: &mut After) -> usize {
        let mut n = 0;
        for piece in &self.pieces {
            let src = piece.filled();
            let take = src.len().min(out.len() - n);
            out[n..n + take].copy_from_slice(&src[..take]);
            n += take;
            if n == out.len() {
                break;
            }
        }
        if !peek {
            self.consume(n, after);
        }
        n
    }

    /// Copies bytes into `bufs`, in order, and consumes them.
    fn copy_vectored(&mut self, bufs: &mut [io::IoSliceMut<'_>], after: &mut After) -> usize {
        let mut n = 0;
        for piece in &self.pieces {
            let copied = copy_vectored_from(piece.filled(), bufs, n);
            n += copied;
            if copied < piece.filled().len() {
                break;
            }
        }
        self.consume(n, after);
        n
    }

    /// Moves bytes onto the end of `buf`, as many as its spare capacity
    /// holds.
    fn append_to(&mut self, buf: &mut Vec<u8>, after: &mut After) -> usize {
        let mut n = 0;
        for piece in &self.pieces {
            let src = piece.filled();
            let copied = crate::buf::append_spare(src, buf);
            n += copied;
            if copied < src.len() {
                break;
            }
        }
        self.consume(n, after);
        n
    }

    /// Lets go of every byte held.
    fn release(&mut self, after: &mut After) {
        for piece in self.pieces.drain(..) {
            piece.release(after);
        }
    }
}

/// Copies `src` into `bufs` after the first `skip` bytes they hold;
/// returns how many bytes it copied.
fn copy_vectored_from(src: &[u8], bufs: &mut [io::IoSliceMut<'_>], mut skip: usize) -> usize {
    let mut n = 0;
    for buf in bufs.iter_mut() {
        if skip >= buf.len() {
            skip -= buf.len();
            continue;
        }
        let room = &mut buf[skip..];
        skip = 0;
        let take = room.len().min(src.len() - n);
        room[..take].copy_from_slice(&src[n..n + take]);
        n += take;
        if n == src.len() {
            break;
        }
    }
    n
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
    accepted: VecDeque<(Owned, Addr)>,
    datagrams: VecDeque<(PoolBuf, Addr)>,
    /// Urgent data or a hangup was reported: a short read may leave bytes
    /// behind it from now on (see [`drained`]).
    unsure: bool,
    /// The source's epoch this state is of.
    epoch: u32,
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

    #[cfg(readiness)]
    /// The driver's part: on readiness, completes what the waiting tasks
    /// asked for, into the pool's buffers, and wakes them.
    pub(super) fn serve(&mut self, fd: RawFd, kind: Kind, edge: bool, after: &mut After) {
        if edge {
            self.blocked = false;
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
        match io {
            #[cfg(readiness)]
            Io::Fd(fd) => self.complete_fd(fd, kind, len, after),
            #[cfg(completion)]
            Io::Ops(route) => self.submit_ops(route, kind, len, after),
            #[cfg(not(completion))]
            Io::None(never, _) => match never {},
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
                Some(Ok(conn)) => self.accepted.push_back(conn),
                Some(Err(err)) => {
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
        self.complete(io, kind, 0, after);
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

    /// What the side holds, as a line, for a test that failed.
    #[cfg(test)]
    pub(super) fn report(&self) -> String {
        format!(
            "lent {:?}, read ahead {}, waiters {}, blocked {}, eof {}, error {:?}",
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

    /// An arrival of the multishot receive, submitted in `epoch`: the
    /// tasks waiting take it. One of an older descriptor only goes back.
    #[cfg(ring)]
    pub(super) fn take_chunk(
        &mut self,
        chunk: crate::rt::shard::recv::RingChunk,
        epoch: u32,
        after: &mut After,
    ) {
        if epoch != self.epoch {
            return;
        }
        self.ahead.push(Piece::Ring(chunk), after);
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
        for (buf, _) in self.datagrams.drain(..) {
            after.drop_buf(buf);
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
        Kind::UnixListener => sys::accept_unix(fd).map(|(s, a)| (s, Addr::Unix(a))),
        _ => sys::accept_inet(fd).map(|(s, a)| (s, Addr::Inet(a))),
    }
}

#[cfg(readiness)]
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
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| {
            st.read(io, kind, out, peek, waker, after)
        })
    }

    /// Polls the read side with `step`, given the way this thread reaches
    /// the descriptor.
    fn poll_reading<R>(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        mut step: impl FnMut(&mut ReadState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        let mut after = After::default();
        let polled = self.with_io(|io| {
            self.read
                .poll(&mut after, cx.waker(), &self.hooks(), |st, after| {
                    step(st, io, cx.waker(), after)
                })
        });
        match polled {
            Ok(poll) => self.finish_poll(after, poll),
            Err(err) => {
                self.settle(after);
                Poll::Ready(Err(err))
            }
        }
    }

    /// Reads into the spare capacity of `buf`, after the bytes it holds,
    /// and gives it back with the read's result. The buffer is lent to the
    /// side until the driver read into it, so the bytes land in it with no
    /// copy (bytes another read left behind are copied in instead).
    pub(crate) async fn read_owned(self: &Arc<Self>, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        let mut op = ReadOwned {
            source: self,
            buf: Some(buf),
            lent: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, op.buf.take().unwrap_or_default())
    }

    fn poll_read_owned(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        held: &mut Option<Vec<u8>>,
    ) -> Poll<io::Result<usize>> {
        if held.as_ref().is_some_and(|b| b.len() == b.capacity()) {
            return Poll::Ready(Ok(0));
        }
        let kind = self.kind;
        let poll = self.poll_reading(cx, |st, io, waker, after| {
            st.read_owned(io, kind, held, waker, after)
        });
        if poll.is_ready() && held.is_none() {
            // The poll failed with the buffer lent: it comes back as it is.
            // If the side is held now, the poll waits and starts over.
            let mut after = After::default();
            let back = self
                .read
                .poll(&mut after, cx.waker(), &self.hooks(), |st, after| {
                    Poll::Ready(st.reclaim(after))
                });
            self.settle(after);
            match back {
                Poll::Ready(buf) => *held = buf,
                Poll::Pending => return Poll::Pending,
            }
        }
        poll
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
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| {
            st.read_vectored(io, kind, bufs, waker, after)
        })
    }

    /// Accepts a connection from a listener.
    pub(crate) fn poll_accept(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(Owned, Addr)>> {
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| st.accept(io, kind, waker, after))
    }

    /// Receives a datagram into `out`, with its sender; `peek` leaves it to
    /// be received again.
    pub(crate) fn poll_receive(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<(usize, Addr)>> {
        let kind = self.kind;
        self.poll_reading(cx, |st, io, waker, after| {
            st.receive(io, kind, out, peek, waker, after)
        })
    }
}

/// A read whose buffer may be lent to the side. Dropped while it is, the
/// side keeps what the driver read into it for the next read.
struct ReadOwned<'a> {
    source: &'a Arc<Source>,
    buf: Option<Vec<u8>>,
    lent: bool,
}

impl ReadOwned<'_> {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let poll = self.source.poll_read_owned(cx, &mut self.buf);
        self.lent = self.buf.is_none();
        poll
    }
}

impl Drop for ReadOwned<'_> {
    fn drop(&mut self) {
        if self.lent {
            let mut after = After::default();
            self.source
                .read
                .defer(super::read_work::ABANDON, &mut after, &self.source.hooks());
            self.source.settle(after);
        }
    }
}
