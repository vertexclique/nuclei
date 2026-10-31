//! The write side of a source: writes, flushes, shutdown, datagram sends
//! and connects.
//!
//! Every call on the descriptor is the driver's ([`WriteState::push`]): on
//! an edge, or at once after a task handed it work while the socket may
//! take it. A task makes the call itself only for a stream write of a
//! buffer big enough (see `direct`) when the side holds nothing. Otherwise
//! a write copies its bytes into a buffer of the pool (the write-behind, one
//! per source) and is accepted; the driver writes them out as the socket
//! takes them. Later writes add to that buffer while it has room and wait
//! while it is full, so bytes go out in the order they were written. A write-behind that fails
//! keeps its error: every write fails with it until a flush reports it,
//! except that a write whose own bytes were the first to fail gets the error
//! itself. A datagram goes to the kernel in its own send's call on unix, so
//! that call answers what the kernel says of it; on a completion port it is
//! copied and queued, one at a time, and an error sending it goes to its own
//! send or else the next one. A connect and a shutdown are the driver's calls
//! too.
//!
//! A stream write may instead lend the side a buffer of its own
//! ([`Source::write_owned`]): the driver writes all of it from there, with
//! no copy, after the write-behind, and the task takes it back once the
//! kernel took the last byte. Writes after it wait until it is out. A lent
//! buffer whose task went away is withdrawn if none of it went out, and
//! written to its end by the driver if some did, so the stream gets it whole
//! or not at all. A zero-copy write ([`Source::write_zero_copy`]) is a lent
//! write whose large chunks the ring sends without the kernel's copy: it
//! ends the same way, and its buffer comes back later, once the kernel let
//! go of it (see `lent`).

use core::future::poll_fn;
use core::task::{Poll, Waker};
use std::io;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::RawFd;
use std::sync::Arc;
use std::task::Context;

use super::{Failure, Io, Kind, Source, Target, Via};
use crate::buf::{Pool, PoolBuf, with_pool_or};
use crate::io::side::{After, Wakers};
use crate::io::sys;

mod calls;
mod datagram;
mod lent;
#[cfg(completion)]
mod ops;
#[cfg(ring)]
mod pinned;

#[cfg(ring)]
use crate::sync::oneshot;
use lent::{Held, Lent};
#[cfg(ring)]
pub(crate) use pinned::Pinned;

/// The least a write-behind buffer holds, so a run of small writes fills
/// one buffer rather than waiting on each.
const BEHIND_MIN: usize = 16 * 1024;

/// The longest datagram every UDP socket takes (over IPv4): only a longer
/// one is held to its socket's own limit before it is queued.
#[cfg(port)]
const UDP_TAKEN: usize = 65_507;

/// A Unix socket's address, borrowed; on Windows, which has none, nothing
/// can be one.
#[cfg(unix)]
pub(crate) type UnixPath<'a> = &'a crate::io::sys::UnixAddr;
#[cfg(windows)]
pub(crate) type UnixPath<'a> = (core::convert::Infallible, core::marker::PhantomData<&'a ()>);

/// Where a datagram goes, borrowed for one send.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Dest<'a> {
    Connected,
    Inet(SocketAddr),
    #[cfg_attr(windows, expect(dead_code, reason = "Windows has no Unix sockets"))]
    Unix(UnixPath<'a>),
}

impl Dest<'_> {
    fn to_owned(self) -> Target {
        match self {
            Dest::Connected => Target::Connected,
            Dest::Inet(addr) => Target::Inet(addr),
            #[cfg(unix)]
            Dest::Unix(addr) => Target::Unix(crate::buf::spare::Spare::new(*addr)),
            #[cfg(windows)]
            Dest::Unix((never, _)) => match never {},
        }
    }
}

#[cfg(readiness)]
impl Target {
    fn as_dest(&self) -> Dest<'_> {
        match self {
            Target::Connected => Dest::Connected,
            Target::Inet(addr) => Dest::Inet(*addr),
            Target::Unix(path) => Dest::Unix(path),
        }
    }
}

/// The peer a connect goes to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Peer<'a> {
    Inet(SocketAddr),
    #[cfg_attr(windows, expect(dead_code, reason = "Windows has no Unix sockets"))]
    Unix(UnixPath<'a>),
}

/// A range of a file the kernel sends to a stream itself: the file's handle, kept open until the
/// send ends, and where the range starts and how long it is.
#[derive(Clone)]
#[cfg_attr(
    unix,
    allow(dead_code, reason = "only Windows sends a file in the kernel here")
)]
pub(crate) struct FileRange {
    pub(crate) handle: Arc<crate::io::raw::OwnedFile>,
    pub(crate) offset: u64,
    pub(crate) len: u32,
}

/// Where a lent write goes besides the stream's own bytes: one datagram's destination, or a range
/// of a file the kernel sends; and whether its large chunks go out zero-copy.
#[derive(Clone, Default)]
pub(crate) struct Lending<'d> {
    pub(crate) to: Option<Dest<'d>>,
    pub(crate) file: Option<FileRange>,
    #[cfg(ring)]
    pub(crate) zero_copy: bool,
}

#[derive(Default)]
pub(super) struct WriteState {
    /// The last call would have blocked, and no edge came since.
    blocked: bool,
    waiters: Wakers,
    /// A write found the pool empty; the source waits for a buffer.
    starved: bool,
    behind: Option<PoolBuf>,
    lent: Option<Lent>,
    queued: Option<(PoolBuf, Target)>,
    /// Why the write-behind stopped.
    failed: Option<Failure>,
    /// An error the next call reports once.
    error: Option<io::Error>,
    /// The connect was started.
    connect_started: bool,
    connecting: bool,
    connected: Option<io::Result<()>>,
    /// The source's epoch this state is of.
    epoch: u32,
    /// Edges (and, on a ring, polls that completed) since the source was
    /// made, as the read side counts them (proofs/tla/asyncfd).
    #[cfg(unix)]
    edges: u32,
    /// The side takes its caller's own calls (as the read side's).
    #[cfg(unix)]
    user_fd: bool,
    /// The id of the operation out with the completion driver; only its
    /// completion clears this.
    #[cfg(completion)]
    out: Option<u64>,
    /// The ring gave an operation back because the socket was not ready:
    /// the next one waits for it to be.
    #[cfg(ring)]
    poll_first: bool,
    /// What the driver gave back undone (bytes of the write-behind it took
    /// part of, or none, or a datagram): it goes out first.
    #[cfg(completion)]
    again: Option<super::ops::Want>,
    /// A connect to start on the completion driver, to its peer.
    #[cfg(completion)]
    dial: Option<super::ops::Dial>,
}

#[cfg(readiness)]
/// Writes bytes to a stream.
fn write_bytes(fd: RawFd, kind: Kind, bytes: &[u8]) -> io::Result<usize> {
    match kind {
        Kind::Bytes | Kind::File => sys::write(fd, bytes),
        _ => sys::send(fd, bytes),
    }
}

#[cfg(readiness)]
/// Sends a datagram.
fn send_datagram(fd: RawFd, bytes: &[u8], dest: Dest<'_>) -> io::Result<usize> {
    match dest {
        Dest::Connected => sys::send(fd, bytes),
        Dest::Inet(addr) => sys::send_to_udp(fd, bytes, addr),
        Dest::Unix(path) => sys::send_to_unix(fd, bytes, path),
    }
}

impl WriteState {
    pub(super) fn epoch_mut(&mut self) -> &mut u32 {
        &mut self.epoch
    }

    /// The id of the operation out with the ring, if one is.
    #[cfg(ring)]
    pub(super) fn out(&self) -> Option<u64> {
        self.out
    }

    /// Bytes written behind that the kernel has not taken: in the
    /// write-behind, or out with the ring (or given back by it).
    #[cfg(test)]
    pub(super) fn has_behind(&self) -> bool {
        self.behind.is_some() || self.ring_holds()
    }

    /// A lent buffer has bytes left to go out.
    fn lent_waits(&self) -> bool {
        self.lent.as_ref().is_some_and(|l| l.done.is_none())
    }

    /// The caller's own calls: `Ready` with the edge count while the side
    /// may be ready, else the task waits (on a ring, for a poll armed now).
    /// What the handle wrote behind goes out first: until it has, the
    /// driver writes it and the task waits; an error it met is reported.
    #[cfg(unix)]
    pub(super) fn fd_ready(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<u32>> {
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        if let Some(failure) = self.failed.take() {
            return Poll::Ready(Err(failure.error()));
        }
        if self.has_unsent() {
            self.user_fd = false;
            self.waiters.register(waker);
            self.push(io, kind, after);
            return Poll::Pending;
        }
        self.user_fd = true;
        if !self.blocked {
            return Poll::Ready(Ok(self.edges));
        }
        self.waiters.register(waker);
        self.push(io, Kind::Fd, after);
        Poll::Pending
    }

    /// The handle's own writes: the driver writes for the side again.
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

    /// Whether a write or send would be taken now: room in the write-behind
    /// (no datagram queued, for a socket of datagrams), or an error, which
    /// stays for it. `Ready` with `true` once writing failed.
    pub(super) fn ready(&mut self, waker: &Waker) -> Poll<bool> {
        if self.failed.is_some() || self.error.is_some() {
            return Poll::Ready(true);
        }
        let room = !self.lent_waits()
            && self.queued.is_none()
            && self
                .behind
                .as_ref()
                .is_none_or(|behind| behind.filled().len() < behind.capacity());
        if room {
            return Poll::Ready(false);
        }
        self.waiters.register(waker);
        Poll::Pending
    }

    /// Whether a stream write of `len` bytes goes straight from the task's
    /// buffer: one big enough (see `direct`), or on a ring any while the pool
    /// has no buffer for the write-behind, so a writer never waits on buffers
    /// that receives of other connections hold (proofs/tla/ring/RingStarve.tla).
    /// A pipe's goes too, while the pool has no buffer for it (`writes_bytes`).
    #[cfg(unix)]
    pub(super) fn writes_itself(&self, io: Io<'_>, len: usize) -> bool {
        #[cfg(ring)]
        if self.starved && matches!(io.via, Via::Ops(_)) {
            return true;
        }
        super::direct::worth(io, len, true)
    }

    /// A stream write of the bytes of `bufs` made straight on the
    /// descriptor `fd`, from the caller's memory, if the side holds nothing
    /// that goes first. The kernel may take part of it, as of any write.
    /// `None` leaves it to the normal path, which writes behind and waits
    /// for readiness when the socket took nothing.
    #[cfg(unix)]
    pub(super) fn write_direct(
        &mut self,
        io: Io<'_>,
        fd: Option<RawFd>,
        kind: Kind,
        bufs: &[io::IoSlice<'_>],
    ) -> Option<io::Result<usize>> {
        let fd = fd?;
        let len: usize = bufs.iter().map(|b| b.len()).sum();
        let waits = matches!(io.via, Via::Fd(_)) && self.blocked;
        if !self.writes_bytes(kind)
            || len == 0
            || !self.writes_itself(io, len)
            || waits
            || self.failed.is_some()
            || self.error.is_some()
            || self.connecting
            || self.behind.is_some()
            || self.lent.is_some()
            || self.queued.is_some()
            || self.ring_holds()
        {
            return None;
        }
        let call = if kind.streams() {
            sys::send_bufs(fd, bufs)
        } else {
            sys::write_bufs(fd, bufs)
        };
        match sys::ready(call) {
            None => {
                if matches!(io.via, Via::Fd(_)) {
                    self.blocked = true;
                }
                None
            }
            Some(Ok(0)) => Some(Err(io::ErrorKind::WriteZero.into())),
            Some(result) => Some(result),
        }
    }

    /// Whether a write of `kind` may go straight from the task's buffer: a
    /// socket's, and while the pool has no buffer for the side a pipe's or a
    /// terminal's, as the read side's `reads_bytes` (so no writer waits on
    /// buffers that receives or reads ahead of others hold).
    #[cfg(unix)]
    fn writes_bytes(&self, kind: Kind) -> bool {
        kind.streams() || (self.starved && kind == Kind::Bytes)
    }

    /// A buffer from this thread's pool, or the side starves until one
    /// comes back. A whole datagram no buffer of the pool could hold fails
    /// the operation instead.
    fn buffer(
        &mut self,
        known: Option<&Pool>,
        len: usize,
        whole: bool,
        after: &mut After,
    ) -> Option<PoolBuf> {
        if whole && let Some(err) = crate::io::side::whole_too_large(known, len) {
            self.error = Some(err);
            return None;
        }
        let buf = with_pool_or(known, |pool| {
            pool.and_then(|p| if whole { p.get_whole(len) } else { p.get(len) })
        });
        if buf.is_none() && !self.starved {
            self.starved = true;
            after.starved = true;
        }
        buf
    }

    #[cfg(readiness)]
    /// The driver's part: on readiness, does what the side holds and wakes
    /// the tasks that wait on any of it.
    pub(super) fn serve(&mut self, fd: RawFd, kind: Kind, edge: bool, after: &mut After) {
        // A call that found the socket full waits for the edge itself (a
        // datagram's, see `send_now`): it is woken even if nothing else moves.
        let unblocked = edge && self.blocked;
        if edge {
            self.blocked = false;
            self.edges = self.edges.wrapping_add(1);
            if self.user_fd {
                after.wake(&mut self.waiters);
                return;
            }
        }
        if self.push_fd(fd, kind, after) || unblocked {
            after.wake(&mut self.waiters);
        }
    }

    /// The driver's part for what the side holds: its calls on the
    /// descriptor, or its next operation on the ring. Returns whether
    /// anything changed at once.
    fn push(&mut self, io: Io<'_>, kind: Kind, after: &mut After) -> bool {
        match io.via {
            #[cfg(readiness)]
            Via::Fd(fd) => self.push_fd(fd, kind, after),
            #[cfg(completion)]
            Via::Ops(route) => {
                self.submit_ops(route, kind, after);
                false
            }
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        }
    }

    /// Bytes, a datagram or a connect the completion driver holds or gave
    /// back.
    #[cfg(completion)]
    fn ring_holds(&self) -> bool {
        self.out.is_some() || self.again.is_some() || self.dial.is_some()
    }

    #[cfg(not(completion))]
    fn ring_holds(&self) -> bool {
        false
    }

    #[cfg(readiness)]
    /// The driver's calls on the descriptor, while the socket takes them:
    /// ends a connect in progress, writes the write-behind out and then the
    /// lent buffer. Returns whether anything changed.
    fn push_fd(&mut self, fd: RawFd, kind: Kind, after: &mut After) -> bool {
        if self.blocked {
            return false;
        }
        let mut progress = false;
        if self.connecting {
            self.connecting = false;
            self.connected = Some(match sys::take_error(fd) {
                Ok(None) => Ok(()),
                Ok(Some(err)) | Err(err) => {
                    crate::rt::counters::io_error(&err);
                    Err(err)
                }
            });
            progress = true;
        }
        if let Some(behind) = self.behind.as_mut() {
            loop {
                match sys::ready(write_bytes(fd, kind, behind.filled())) {
                    None => {
                        self.blocked = true;
                        break;
                    }
                    Some(Ok(0)) => {
                        self.failed = Some(Failure::of(&io::ErrorKind::WriteZero.into()));
                        break;
                    }
                    Some(Ok(n)) => {
                        behind.consume(n);
                        progress = true;
                        if behind.is_empty() {
                            break;
                        }
                    }
                    Some(Err(err)) => {
                        self.failed = Some(Failure::of(&err));
                        break;
                    }
                }
            }
            if self.failed.is_some() || self.behind.as_ref().is_some_and(PoolBuf::is_empty) {
                progress = true;
                if let Some(done) = self.behind.take() {
                    after.drop_buf(done);
                }
            }
        }
        if self.behind.is_none() && self.lent_waits() {
            progress |= self.push_lent(fd, kind, after);
        }
        progress
    }

    /// A write of the bytes of `bufs`, in order (one slice for a plain
    /// write): copied into the write-behind, which the driver writes out at
    /// once if the socket may take it.
    fn write(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        bufs: &[io::IoSlice<'_>],
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if let Some(failure) = self.failed {
            return Poll::Ready(Err(failure.error()));
        }
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        let len: usize = bufs.iter().map(|b| b.len()).sum();
        if len == 0 {
            return Poll::Ready(Ok(0));
        }
        if self.lent_waits() {
            // A lent buffer goes out first.
            self.waiters.register(waker);
            return Poll::Pending;
        }
        let first = self.behind.is_none();
        let n = match self.behind.as_mut() {
            Some(behind) => behind.append_vectored(bufs),
            None => match self.buffer(io.pool(), len.max(BEHIND_MIN), false, after) {
                Some(mut behind) => {
                    let n = behind.append_vectored(bufs);
                    self.behind = Some(behind);
                    n
                }
                None => 0,
            },
        };
        if n == 0 {
            // No room (the write-behind is full, or the pool has nothing):
            // the write waits for the driver to make some. On a ring a write
            // the pool has nothing for waits instead for a poll that says the
            // socket takes bytes, and then writes them itself
            // (`writes_itself`).
            #[cfg(ring)]
            if self.starved && self.behind.is_none() && matches!(io.via, Via::Ops(_)) {
                self.poll_first = true;
                self.push(io, kind, after);
            }
            // On epoll and kqueue a pipe's write the pool has nothing for is
            // the task's own call now (`writes_bytes`); one the pipe cannot
            // take waits for its edge.
            #[cfg(readiness)]
            if let Via::Fd(fd) = io.via
                && self.behind.is_none()
                && let Some(result) = self.write_direct(io, Some(fd), kind, bufs)
            {
                return Poll::Ready(result);
            }
            // On a port such a write sends from the write slot's own room
            // instead, a few hundred bytes at a time
            // (proofs/tla/port/PortStarve.tla).
            #[cfg(port)]
            if let Some(n) = self.write_room(io, kind, bufs, after) {
                return Poll::Ready(Ok(n));
            }
            self.waiters.register(waker);
            return Poll::Pending;
        }
        self.push(io, kind, after);
        if first && let Some(failure) = self.failed.take() {
            // Nothing written before was lost: the error is this write's.
            return Poll::Ready(Err(failure.error()));
        }
        if self.behind.is_some() {
            after.register = true;
        }
        Poll::Ready(Ok(n))
    }

    /// On a port, a write of a stream or a pipe the pool had no buffer for:
    /// the front of `bufs` is copied into the write slot's own room and sent
    /// from there, so no writer waits on buffers that reads ahead or receives
    /// of others hold. `None` while the side holds bytes that go first.
    #[cfg(port)]
    fn write_room(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        bufs: &[io::IoSlice<'_>],
        after: &mut After,
    ) -> Option<usize> {
        let bytes = kind.streams() || kind == Kind::Bytes;
        if !self.starved || !bytes || self.behind.is_some() || self.ring_holds() {
            return None;
        }
        let Via::Ops(route) = io.via;
        let n = route.issuer(after)?.fill_room(bufs);
        self.again = Some(super::ops::Want::SendRoom { from: 0, to: n });
        self.push(io, kind, after);
        Some(n)
    }

    fn flush(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<()>> {
        self.push(io, kind, after);
        if let Some(failure) = self.failed.take() {
            return Poll::Ready(Err(failure.error()));
        }
        if self.behind.is_some() || self.lent_waits() || self.ring_holds() {
            self.waiters.register(waker);
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }

    /// Flushes, then shuts the halves `how` names (the writing half, or
    /// both) of a stream's `fd` down (with nothing out on the ring by then,
    /// the driver's call on it at once). Every close asks the kernel, so one
    /// after the first answers what the system answers to a second shutdown
    /// (macOS: `ENOTCONN`), as std's.
    fn close(
        &mut self,
        io: Io<'_>,
        fd: crate::io::raw::Raw,
        kind: Kind,
        how: std::net::Shutdown,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<()>> {
        match self.flush(io, kind, waker, after) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        if kind.streams() {
            return Poll::Ready(sys::shutdown(fd, how).inspect_err(crate::rt::counters::io_error));
        }
        Poll::Ready(Ok(()))
    }

    /// A datagram to `dest`. On unix the polling task hands it to the kernel
    /// itself (see [`send_now`](Self::send_now)); on a completion port, or a
    /// ring source with no descriptor (`fd`), it is copied and queued, and
    /// sent by the driver.
    #[cfg_attr(
        not(completion),
        expect(
            unused_variables,
            reason = "a readiness driver's descriptor comes with `io`"
        )
    )]
    #[expect(
        clippy::too_many_arguments,
        reason = "the step's context, as every step's"
    )]
    fn send(
        &mut self,
        io: Io<'_>,
        fd: Option<crate::io::raw::Raw>,
        kind: Kind,
        bytes: &[u8],
        dest: Dest<'_>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        if self.queued.is_some() || self.lent_waits() {
            self.waiters.register(waker);
            return Poll::Pending;
        }
        match io.via {
            #[cfg(readiness)]
            Via::Fd(fd) => self.send_now(io, fd, kind, bytes, dest, waker, after),
            #[cfg(ring)]
            Via::Ops(_) => match fd {
                Some(fd) => self.send_now(io, fd, kind, bytes, dest, waker, after),
                None => self.send_queued(io, kind, bytes, dest, waker, after),
            },
            #[cfg(port)]
            Via::Ops(_) => {
                // Refused at once, as std's call is, not queued for the next
                // call to hear of.
                if bytes.len() > UDP_TAKEN
                    && let Some(err) = fd.and_then(|fd| sys::datagram_too_long(fd, bytes.len()))
                {
                    return Poll::Ready(Err(err));
                }
                let queued = self.send_queued(io, kind, bytes, dest, waker, after);
                // The pool had no buffer for it: with nothing of the socket's
                // in the kernel, the task hands it over itself
                // (proofs/tla/port/PortStarve.tla).
                if queued.is_pending()
                    && self.starved
                    && !self.ring_holds()
                    && let Some(fd) = fd
                {
                    let to = match dest {
                        Dest::Connected => None,
                        Dest::Inet(addr) => Some(addr),
                        Dest::Unix((never, _)) => match never {},
                    };
                    return Poll::Ready(sys::send_datagram_now(fd, bytes, to.as_ref()));
                }
                queued
            }
            #[cfg(not(completion))]
            Via::None(never, _) => match never {},
        }
    }

    /// Bytes or a datagram accepted and not handed to the kernel yet, that
    /// the driver can still deliver.
    pub(super) fn has_unsent(&self) -> bool {
        self.failed.is_none()
            && (self.behind.is_some()
                || self.lent_waits()
                || self.queued.is_some()
                || self.ring_holds())
    }

    /// What the side holds, as a line, for a test that failed.
    #[cfg(test)]
    pub(super) fn report(&self) -> String {
        #[cfg(completion)]
        let ops = format!(
            ", out {:?}, again {}, dial {}",
            self.out,
            self.again.is_some(),
            self.dial.is_some()
        );
        #[cfg(not(completion))]
        let ops = String::new();
        #[cfg(ring)]
        let ops = format!("{ops}, poll first {}", self.poll_first);
        format!(
            "behind {:?}, lent {}, queued {}, waiters {}, blocked {}, starved {}, failed {}, epoch {}{ops}",
            self.behind.as_ref().map(|b| b.filled().len()),
            self.lent.is_some(),
            self.queued.is_some(),
            self.waiters.len(),
            self.blocked,
            self.starved,
            self.failed.is_some(),
            self.epoch,
        )
    }

    fn drain(&mut self, io: Io<'_>, kind: Kind, waker: &Waker, after: &mut After) -> Poll<()> {
        self.push(io, kind, after);
        if self.has_unsent() {
            self.waiters.register(waker);
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    /// A connect to `peer`: the driver starts it on the first poll, and sees
    /// it end on the socket's first edge.
    #[cfg_attr(
        not(completion),
        expect(
            unused_variables,
            reason = "the kind matters to a completion driver's connect only"
        )
    )]
    fn connect(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        peer: Peer<'_>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<()>> {
        if let Some(result) = self.connected.take() {
            // A connect that failed is forgotten, so the next poll makes
            // another (a Unix connect waits out a full backlog so).
            self.connect_started &= result.is_ok();
            return Poll::Ready(result);
        }
        if !self.connect_started {
            self.connect_started = true;
            match io.via {
                #[cfg(readiness)]
                Via::Fd(fd) => {
                    let started = match peer {
                        Peer::Inet(addr) => sys::connect_inet(fd, &addr),
                        Peer::Unix(path) => sys::connect_unix(fd, path),
                    };
                    match started {
                        Ok(true) => return Poll::Ready(Ok(())),
                        Ok(false) => {
                            self.connecting = true;
                            after.register = true;
                        }
                        Err(err) => {
                            self.connect_started = false;
                            crate::rt::counters::io_error(&err);
                            return Poll::Ready(Err(err));
                        }
                    }
                }
                #[cfg(completion)]
                Via::Ops(route) => {
                    self.dial = Some(match peer {
                        Peer::Inet(addr) => super::ops::Dial::Inet(addr),
                        #[cfg(unix)]
                        Peer::Unix(addr) => {
                            super::ops::Dial::Unix(crate::buf::spare::Spare::new(*addr))
                        }
                        #[cfg(windows)]
                        Peer::Unix((never, _)) => match never {},
                    });
                    self.connecting = true;
                    self.submit_ops(route, kind, after);
                }
            }
        }
        if self.connecting {
            self.waiters.register(waker);
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }

    /// Registration failed: the waiting tasks get the error.
    pub(super) fn fail(&mut self, failure: Failure, after: &mut After) {
        if self.connecting {
            self.connecting = false;
            self.connected = Some(Err(failure.error()));
        } else {
            self.error = Some(failure.error());
        }
        if let Some(lent) = self.lent.as_mut().filter(|l| l.done.is_none()) {
            lent.done = Some(Err(failure.error()));
            if let Some(w) = lent.waker.take() {
                after.wake_one(w);
            }
        }
        after.wake(&mut self.waiters);
    }

    /// A buffer came back to the pool a write found empty.
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

    /// Drops what the side holds: the descriptor it was for is gone.
    pub(super) fn discard(&mut self, after: &mut After) {
        if let Some(buf) = self.behind.take() {
            after.drop_buf(buf);
        }
        if let Some((buf, _)) = self.queued.take() {
            after.drop_buf(buf);
        }
        if let Some(w) = self.lent.take().and_then(|l| l.waker) {
            after.forget_one(w);
        }
        self.failed = None;
        self.error = None;
        self.connect_started = false;
        self.connecting = false;
        self.connected = None;
        self.blocked = false;
        #[cfg(ring)]
        {
            self.poll_first = false;
        }
        #[cfg(completion)]
        {
            if let Some(want) = self.again.take() {
                want.release(after);
            }
            self.dial = None;
        }
    }
}
