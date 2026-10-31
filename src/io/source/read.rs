//! The read side of a source: reads, peeks, accepts and datagram receives.
//!
//! Every call on the descriptor is the driver's ([`ReadState::complete`]),
//! into buffers of the pool: on an edge, for the tasks that wait, or at
//! once when a task asks and the socket may hold something. A task takes
//! what the driver completed, copying it into its own buffer; only a
//! stream read into a buffer big enough (see `direct`) makes the call
//! itself, into that buffer, when the read-ahead holds nothing, and so does
//! a datagram receive on unix (on a ring, a Unix datagram's: a UDP datagram
//! fits a buffer of the pool's largest class), so no datagram is cut to a
//! buffer of the pool. What no task took yet waits here: a stream's
//! read-ahead (in the order it came: a buffer of the pool, a lent buffer's
//! leftover, or on a ring the buffers of the shard's receive group that a
//! multishot receive handed out), its end or its error, a listener's
//! accepted connections, a completion driver's datagrams. A task whose future
//! went away leaves it for the next operation, so nothing the kernel handed
//! over is lost.
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
#[cfg(unix)]
mod datagram;
#[cfg(completion)]
mod ops;

use ahead::{Ahead, Piece};

/// The most connections the driver takes ahead on one edge.
#[cfg(readiness)]
const BATCH: usize = 32;

/// A datagram received ahead: by a completion driver in a buffer of the
/// pool, on a ring in a buffer of the receive group a multishot receive
/// filled, or in a task's own buffer whose task went away after the driver
/// read into it.
enum Datagram {
    #[cfg(completion)]
    Pool(PoolBuf),
    #[cfg(ring)]
    Ring(crate::rt::shard::recv::RingChunk),
    Owned {
        buf: Vec<u8>,
        start: usize,
    },
}

impl Datagram {
    fn bytes(&self) -> &[u8] {
        match self {
            #[cfg(completion)]
            Datagram::Pool(buf) => buf.filled(),
            #[cfg(ring)]
            Datagram::Ring(chunk) => chunk.bytes(),
            Datagram::Owned { buf, start } => buf.get(*start..).unwrap_or_default(),
        }
    }

    /// Copies the datagram into `out`, cut to it as the kernel cuts it.
    fn peek_into(&self, out: &mut [u8]) -> usize {
        let src = self.bytes();
        let n = src.len().min(out.len());
        out[..n].copy_from_slice(&src[..n]);
        n
    }

    /// Appends the datagram to `out`, cut to its spare capacity as the
    /// kernel cuts it.
    fn append_to(&self, out: &mut Vec<u8>) -> usize {
        let src = self.bytes();
        let n = src.len().min(out.capacity() - out.len());
        out.extend_from_slice(&src[..n]);
        n
    }

    #[cfg_attr(
        not(completion),
        expect(
            unused_variables,
            reason = "only a pool's buffer goes back through `after`"
        )
    )]
    fn release(self, after: &mut After) {
        match self {
            #[cfg(completion)]
            Datagram::Pool(buf) => after.drop_buf(buf),
            // Goes back to its group as it drops.
            #[cfg(ring)]
            Datagram::Ring(_) => {}
            Datagram::Owned { .. } => {}
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
    /// The sender, once a datagram was read into the buffer.
    peer: Option<Addr>,
}

/// The longest datagram UDP carries, by its length field: what a datagram
/// received ahead needs room for whole.
#[cfg(completion)]
const UDP_MAX: usize = u16::MAX as usize;

/// Whether `TCP_INQ` is on for a socket (see `ReadState::inq_wanted`).
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Inq {
    /// Not asked yet.
    #[default]
    Unknown,
    On,
    /// The kernel does not know the option.
    Off,
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
    /// Whether the kernel counts what a read of this TCP socket leaves.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    inq: Inq,
    /// The last accept failed for want of a descriptor: the next one looks
    /// for a free one first (see [`ReadState::accept_checked`]).
    #[cfg(readiness)]
    short_of_fds: bool,
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
    /// A task that receives a datagram itself waits for the poll to say one
    /// came (see `receive_now`): its end wakes the tasks.
    #[cfg(ring)]
    task_polls: bool,
    /// On a ring, the poll of a side the pool had no buffer for said bytes
    /// came: its task reads them into its own buffer, and the ring asks for
    /// nothing more until it has (proofs/tla/ring/RingStarve.tla). On a
    /// port, a zero-byte receive said bytes or the end came: the next
    /// receive takes a buffer of the pool and ends at once, or, with none,
    /// the task reads them itself (proofs/tla/port/PortStarve.tla).
    #[cfg(completion)]
    readable: bool,

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

    /// A lent buffer waits for the driver, and no operation has it: its
    /// task may take it back.
    fn lent_idle(&self) -> bool {
        self.lent
            .as_ref()
            .is_some_and(|l| !l.out && l.done.is_none())
    }

    /// A lent buffer waits for the driver.
    fn lent_waits(&self) -> bool {
        self.lent.as_ref().is_some_and(|l| l.done.is_none())
    }

    #[inline(always)]
    fn wait(&mut self, waker: &Waker, len: usize) {
        self.waiters.register(waker);
        self.want = self.want.max(len);
    }

    /// Whether a read, receive or accept would end now, taking nothing:
    /// what came ahead, the end, or an error, which stays for it. `Ready`
    /// with `true` at the end of the stream.
    #[cfg_attr(
        not(unix),
        expect(
            unused_variables,
            reason = "only unix tasks receive datagrams themselves"
        )
    )]
    pub(super) fn ready(
        &mut self,
        io: Io<'_>,
        fd: Option<crate::io::raw::Raw>,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<bool> {
        #[cfg(unix)]
        if kind.datagrams()
            && let Some(fd) = self.task_receives(io, fd, kind)
        {
            return self.datagram_ready(io, fd, kind, waker, after);
        }
        // Bytes came for a starved side: its task's read takes them.
        #[cfg(completion)]
        if self.starved && self.readable {
            return Poll::Ready(self.eof);
        }
        // A datagram read ahead (on a completion driver, of UDP) is read
        // whole: the receive that takes it may ask for any size, and one read
        // into less would be cut. The pool's largest buffer holds any
        // datagram UDP carries; a pool too small for one reads into what it
        // has, as a receive would.
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
        #[cfg(readiness)]
        if let Some(ready) = self.starved_look(io, kind) {
            return ready;
        }
        self.wait(waker, len);
        Poll::Pending
    }

    /// On a readiness driver, whether a stream socket the pool has no buffer
    /// for may be read now, by a peek at one byte straight from it that takes
    /// nothing: its task's read goes straight into its own buffer
    /// (`reads_itself`). `None` once the socket is found empty, which waits
    /// for the next edge, and for a side that is not starved
    /// (proofs/tla/starve/FdStarve.tla).
    #[cfg(readiness)]
    fn starved_look(&mut self, io: Io<'_>, kind: Kind) -> Option<Poll<bool>> {
        let fd = match io.via {
            Via::Fd(fd) => fd,
            #[cfg(completion)]
            Via::Ops(_) => return None,
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        };
        if !self.starved || self.blocked || !kind.streams() {
            return None;
        }
        match sys::ready(sys::peek(fd, &mut [0; 1])) {
            None => {
                self.blocked = true;
                None
            }
            Some(Ok(0)) => {
                self.eof = true;
                Some(Poll::Ready(true))
            }
            Some(Ok(_)) => Some(Poll::Ready(false)),
            Some(Err(err)) => {
                self.error = Some(err);
                Some(Poll::Ready(false))
            }
        }
    }

    /// On a readiness driver, a side the pool just had no buffer for whose
    /// socket may hold bytes no edge will report again: its waiting tasks are
    /// woken to read them into their own buffers (`reads_itself`), and one
    /// that finds the socket empty waits for the next edge
    /// (proofs/tla/starve/FdStarve.tla).
    #[cfg_attr(
        not(readiness),
        expect(unused_variables, reason = "only a readiness driver looks again")
    )]
    #[inline(always)]
    fn look_again(&mut self, io: Io<'_>, after: &mut After) {
        #[cfg(readiness)]
        if self.starved && !self.blocked && matches!(io.via, Via::Fd(_)) {
            after.wake(&mut self.waiters);
        }
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
    #[inline]
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
        if self.leaves_to_tasks(kind) {
            // The task reads into its own buffer when it polls.
            self.want = 0;
            after.wake(&mut self.waiters);
            return;
        }
        self.complete_fd(fd, kind, self.want, after);
        // A side the pool had no buffer for reads its bytes itself.
        if self.has_result() || self.starved {
            self.want = 0;
            after.wake(&mut self.waiters);
        }
    }

    /// The driver's call on the descriptor: a stream's bytes, into the lent
    /// buffer if there is one, else into the pool's (`len` asked); a
    /// listener's connections; a socket's datagrams (`len` each). Nothing
    /// while the last call would have blocked and no edge came since, or
    /// while a result waits to be taken.
    #[inline]
    fn complete(&mut self, io: Io<'_>, kind: Kind, len: usize, after: &mut After) {
        match io.via {
            #[cfg(readiness)]
            Via::Fd(fd) => self.complete_fd(fd, kind, len, after),
            #[cfg(completion)]
            Via::Ops(route) => {
                if !self.armed_and_quiet() {
                    self.submit_ops(route, kind, len, after);
                }
            }
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        }
    }

    /// A multishot receive is out and stays: no lent buffer waits for the next bytes (which would
    /// stop it) and the caller makes no calls of its own, so `submit_ops` would find it out and
    /// do nothing.
    #[cfg(completion)]
    fn armed_and_quiet(&self) -> bool {
        #[cfg(ring)]
        return self.out.is_some()
            && self.multi
            && !self.multi_stopping
            && !self.user_fd
            && !(self.ahead.is_empty()
                && self
                    .lent
                    .as_ref()
                    .is_some_and(|l| !l.out && l.done.is_none()));
        #[cfg(not(ring))]
        false
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
            Kind::Udp | Kind::UnixDatagram if self.lent.is_some() => {
                self.receive_lent(fd, kind, after);
            }
            // The task receives a datagram itself (`receive_now`).
            Kind::Udp | Kind::UnixDatagram => {}
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
    #[inline(always)]
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
    /// Receives one datagram into the spare capacity of the lent buffer, and
    /// wakes its task.
    fn receive_lent(&mut self, fd: RawFd, kind: Kind, after: &mut After) {
        let Some(lent) = self.lent.as_mut() else {
            return;
        };
        let spare = lent.buf.spare_capacity_mut();
        let result = match kind {
            Kind::UnixDatagram => sys::ready(sys::recv_from_unix_uninit(fd, spare, false))
                .map(|r| r.map(|(n, a)| (n, Addr::Unix(crate::buf::spare::Spare::new(a))))),
            _ => sys::ready(sys::recv_from_udp_uninit(fd, spare, false))
                .map(|r| r.map(|(n, a)| (n, Addr::Inet(a)))),
        };
        let Some(result) = result else {
            self.blocked = true;
            return;
        };
        lent.done = Some(result.map(|(n, from)| {
            // SAFETY: the kernel wrote the first `n` bytes of the spare
            // capacity.
            unsafe { lent.buf.set_len(lent.buf.len() + n) };
            lent.peer = Some(from);
            n
        }));
        if let Some(waker) = lent.waker.take() {
            after.wake_one(waker);
        }
    }

    #[cfg(readiness)]
    fn accept_ahead(&mut self, fd: RawFd, kind: Kind) {
        let limit = self.waiters.len().clamp(1, BATCH);
        while self.accepted.len() < limit {
            match sys::ready(self.accept_checked(fd, kind)) {
                None => {
                    self.blocked = true;
                    break;
                }
                Some(Ok((conn, peer))) => {
                    self.short_of_fds = false;
                    self.accepted.push_back((conn, Peer::Known(peer)));
                }
                Some(Err(err)) => {
                    crate::rt::counters::accept_error(&err);
                    self.short_of_fds = crate::rt::counters::out_of_descriptors(&err);
                    self.error = Some(err);
                    break;
                }
            }
        }
    }

    /// An accept from the listener `fd`; after one that failed for want of a
    /// descriptor, only once a copy of `fd` (closed at once) says one is free.
    /// On macOS an accept that finds none closes the connection it took off
    /// the backlog (XNU's `accept_nocancel`, bsd/kern/uipc_syscalls.c), so a
    /// retry while none is free would close one connection each; the copy's
    /// EMFILE leaves the backlog whole (proofs/tla/accept).
    #[cfg(readiness)]
    fn accept_checked(&self, fd: RawFd, kind: Kind) -> io::Result<(Owned, Addr)> {
        if self.short_of_fds {
            drop(sys::duplicate(fd)?);
        }
        accept(fd, kind)
    }

    /// The tasks waiting asked for buffers big enough to read into directly,
    /// or the pool had no buffer for the side (`reads_itself`), and nothing is
    /// held that must come first.
    #[cfg(readiness)]
    fn leaves_to_tasks(&self, kind: Kind) -> bool {
        let theirs = if kind.datagrams() {
            // A datagram is always the task's to receive (`receive_now`).
            self.datagrams.is_empty()
        } else {
            self.reads_bytes(kind)
                && (self.want >= super::direct::READ_READINESS
                    || self.starved
                    || self.inq_wanted(kind))
                && self.ahead.is_empty()
        };
        theirs && self.lent.is_none() && !self.eof && self.error.is_none()
    }

    /// Whether a stream read (or `peek`) of `len` bytes goes straight into
    /// the task's buffer: a read big enough (see `direct`), or any while the
    /// pool has no buffer for the side, so a reader never waits on buffers
    /// that receives or reads ahead of other connections hold
    /// (proofs/tla/ring/RingStarve.tla, proofs/tla/starve/FdStarve.tla).
    #[cfg(unix)]
    pub(super) fn reads_itself(&self, io: Io<'_>, kind: Kind, len: usize, peek: bool) -> bool {
        self.starved
            || (!peek
                && ((matches!(io.via, Via::Fd(_)) && self.inq_wanted(kind))
                    || super::direct::worth(io, len, false)))
    }

    /// Whether a TCP read on a readiness driver goes straight into the
    /// task's buffer at every size: the kernel counts what a read leaves
    /// (`TCP_INQ`), so even a read that filled its buffer knows whether the
    /// socket is empty, and the next one waits for an edge instead of
    /// finding nothing (proofs/tla/starve/FdStarve.tla). Not once the kernel
    /// refused the option, and not off Linux.
    #[cfg(unix)]
    pub(super) fn inq_wanted(&self, kind: Kind) -> bool {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return kind == Kind::Tcp && self.inq != Inq::Off;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = kind;
            false
        }
    }

    /// Whether the receives of `fd` report what they leave: asks the kernel
    /// the first time.
    #[cfg(unix)]
    pub(super) fn inq_on(&mut self, fd: RawFd) -> bool {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            if self.inq == Inq::Unknown {
                self.inq = if sys::enable_inq(fd) {
                    Inq::On
                } else {
                    Inq::Off
                };
            }
            self.inq == Inq::On
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = fd;
            false
        }
    }

    /// Whether a read of `kind` may go straight into the task's buffer: a
    /// socket's, and while the pool has no buffer for the side any byte
    /// stream's (a pipe's, a terminal's).
    #[cfg(unix)]
    fn reads_bytes(&self, kind: Kind) -> bool {
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
    pub(super) fn read_direct(
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
        let inq = !peek && matches!(io.via, Via::Fd(_)) && self.inq_wanted(kind) && self.inq_on(fd);
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

    /// A stream read (or peek) into `out`: the driver reads into the pool
    /// first if the socket may hold bytes, and the task copies them out.
    #[inline]
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
            ahead.copy_into(out, peek, after, io.shard)
        }) {
            Some(result) => {
                self.pass_on(after);
                Poll::Ready(result)
            }
            None => {
                #[cfg(port)]
                if let Some(result) = self.read_starved(io, out, peek) {
                    return Poll::Ready(result);
                }
                self.wait(waker, out.len());
                self.look_again(io, after);
                Poll::Pending
            }
        }
    }

    /// On a port, a stream read (or `peek`) of a side the pool has no
    /// buffer for, whose bytes a zero-byte receive said came: straight from
    /// the socket into `out`, which holds them, so the call takes them at
    /// once (proofs/tla/port/PortStarve.tla). `None` for any other side.
    #[cfg(port)]
    fn read_starved(
        &mut self,
        io: Io<'_>,
        out: &mut [u8],
        peek: bool,
    ) -> Option<io::Result<usize>> {
        if !self.starved
            || !self.readable
            || self.out.is_some()
            || !self.ahead.is_empty()
            || self.lent.is_some()
            || self.eof
            || self.error.is_some()
        {
            return None;
        }
        let Via::Ops(route) = io.via;
        let result = crate::io::sys::recv_ready(route.socket()?, out, peek);
        // A look leaves the bytes; a read may take them all, and the next
        // waits for more in a zero-byte receive.
        self.readable = peek && matches!(result, Ok(1..));
        Some(result)
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
            ahead.copy_vectored(bufs, after, io.shard)
        }) {
            Some(result) => {
                self.pass_on(after);
                Poll::Ready(result)
            }
            None => {
                #[cfg(port)]
                if let Some(first) = bufs.iter_mut().find(|b| !b.is_empty())
                    && let Some(result) = self.read_starved(io, first, false)
                {
                    return Poll::Ready(result);
                }
                self.wait(waker, len);
                self.look_again(io, after);
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
        if held.is_none() && self.has_result() && self.lent_idle() {
            // An operation out before the buffer was lent brought bytes, the
            // end or an error to the read-ahead: they go first, taken into
            // the buffer.
            *held = self.reclaim(after);
        }
        if let Some(buf) = held.as_mut() {
            if let Some(result) = self.take_stream(false, after, |ahead, after| {
                ahead.append_to(buf, after, io.shard)
            }) {
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
                peer: None,
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

    /// A datagram receive into a task's own buffer (`held`; `None` while it
    /// is lent), as [`read_owned`](Self::read_owned) reads a stream: a
    /// datagram that came before is copied into it; otherwise it is lent and
    /// the driver receives into it. The sender goes to `from`.
    fn read_owned_from(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        held: &mut Option<Vec<u8>>,
        from: &mut Option<Addr>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if held.is_none() && !self.datagrams.is_empty() && self.lent_idle() {
            // One came while this task's buffer waited: it goes first. Only
            // the task whose buffer is lent (it holds none) takes it back
            // (proofs/tla/lent/LentDatagram.tla); another task's receive
            // takes the datagram into its own.
            *held = self.reclaim(after);
        }
        if let Some(buf) = held.as_mut() {
            if let Some((datagram, addr)) = self.datagrams.pop_front() {
                let n = datagram.append_to(buf);
                datagram.release(after);
                *from = Some(addr);
                self.pass_on(after);
                return Poll::Ready(Ok(n));
            }
            if let Some(err) = self.error.take() {
                return Poll::Ready(Err(err));
            }
            if self.lent.is_some() {
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
                peer: None,
            });
        }
        let old = self.lent.as_mut().and_then(|l| l.waker.take());
        self.complete(io, kind, 0, after);
        let Some(lent) = self.lent.as_mut() else {
            return Poll::Ready(Err(io::Error::other("the receive lost its buffer")));
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
        let Some(Lent {
            buf, done, peer, ..
        }) = self.lent.take()
        else {
            return Poll::Ready(Err(io::Error::other("the receive lost its buffer")));
        };
        *held = Some(buf);
        *from = peer;
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
            // A datagram, even an empty one, is received whole: the next
            // receive takes it.
            Some(Ok(_)) if lent.peer.is_some() => {
                if let Some(peer) = lent.peer {
                    self.datagrams.push_back((
                        Datagram::Owned {
                            buf: lent.buf,
                            start: lent.from,
                        },
                        peer,
                    ));
                }
            }
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

    /// A datagram receive (or peek) into `out`: on unix the task's own (see
    /// [`receive_now`](Self::receive_now)); else the driver receives into
    /// the pool first if the socket may hold one, and the task copies it out
    /// (what does not fit `out` is cut, as the kernel cuts it).
    #[cfg_attr(
        not(unix),
        expect(
            unused_variables,
            reason = "only unix tasks receive datagrams themselves"
        )
    )]
    #[expect(
        clippy::too_many_arguments,
        reason = "the step's context, as every step's"
    )]
    fn receive(
        &mut self,
        io: Io<'_>,
        fd: Option<crate::io::raw::Raw>,
        kind: Kind,
        out: &mut [u8],
        peek: bool,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<(usize, Addr)>> {
        #[cfg(unix)]
        if let Some(poll) = self.receive_now(io, fd, kind, out, peek, waker, after) {
            return poll;
        }
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
                #[cfg(port)]
                if let Some(result) = self.receive_starved(io, out, peek) {
                    return Poll::Ready(result);
                }
                self.wait(waker, out.len());
                Poll::Pending
            }
        }
    }

    /// On a port, a datagram receive (or `peek`) of a side the pool has no
    /// buffer for, once a zero-byte look said one came: straight from the
    /// socket into `out`, cut to it as the pool's receive cuts it, so the
    /// call takes it at once (proofs/tla/port/PortStarve.tla). `None` for any
    /// other side.
    #[cfg(port)]
    fn receive_starved(
        &mut self,
        io: Io<'_>,
        out: &mut [u8],
        peek: bool,
    ) -> Option<io::Result<(usize, Addr)>> {
        if !self.starved
            || !self.readable
            || self.out.is_some()
            || !self.datagrams.is_empty()
            || self.lent.is_some()
            || self.error.is_some()
        {
            return None;
        }
        let Via::Ops(route) = io.via;
        let result = crate::io::sys::recv_from_ready(route.socket()?, out, peek);
        self.readable = peek && result.is_ok();
        Some(result.map(|(n, from)| (n, Addr::Inet(from))))
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
            #[cfg(ring)]
            {
                self.readable = false;
            }
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
    #[inline(always)]
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
            self.task_polls = false;
        }
        #[cfg(completion)]
        {
            self.readable = false;
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
