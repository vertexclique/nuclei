//! The write side of a source: writes, flushes, shutdown, datagram sends
//! and connects.
//!
//! Every call on the descriptor is the driver's ([`WriteState::push`]): on
//! an edge, or at once after a task handed it work while the socket may
//! take it. A task never makes the call itself. A write copies its bytes
//! into a buffer of the pool (the write-behind, one per source) and is
//! accepted; the driver writes them out as the socket takes them. Later
//! writes add to that buffer while it has room and wait while it is full,
//! so bytes go out in the order they were written. A write-behind that fails
//! keeps its error: every write fails with it until a flush reports it,
//! except that a write whose own bytes were the first to fail gets the error
//! itself. A datagram is copied and queued the same way, one at a time, and
//! an error sending it goes to its own send or else the next one. A connect
//! and a shutdown are the driver's calls too.
//!
//! A stream write may instead lend the side a buffer of its own
//! ([`Source::write_owned`]): the driver writes all of it from there, with
//! no copy, after the write-behind, and the task takes it back once the
//! kernel took the last byte. Writes after it wait until it is out. A lent
//! buffer whose task went away is withdrawn if none of it went out, and
//! written to its end by the driver if some did, so the stream gets it whole
//! or not at all.

use core::future::poll_fn;
use core::task::{Poll, Waker};
use std::io;
use std::net::SocketAddr;
#[cfg(readiness)]
use std::os::fd::RawFd;
use std::sync::Arc;
use std::task::Context;

use super::{Failure, Io, Kind, Source, Target};
use crate::buf::{PoolBuf, with_pool};
use crate::io::side::{After, Wakers};
use crate::io::sys;

#[cfg(completion)]
mod ops;

/// The least a write-behind buffer holds, so a run of small writes fills
/// one buffer rather than waiting on each.
const BEHIND_MIN: usize = 16 * 1024;

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
            Dest::Unix(addr) => Target::Unix(*addr),
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

/// A task's own buffer, lent to the side for the driver to write out.
struct Lent {
    buf: Vec<u8>,
    /// Bytes the kernel took.
    sent: usize,
    /// The write's result, once all of it went out or it failed.
    done: Option<io::Result<usize>>,
    /// The task to wake when it is done; `None` while the task polls.
    waker: Option<Waker>,
    /// The buffer is out with a ring operation (`buf` is empty meanwhile).
    out: bool,
    /// The task went away after some of the buffer went out, or while it
    /// was out: the driver writes the rest, then drops it.
    // vertexia: the caller's allocation, kept uncharged by the pool (one per
    // source at most, for at most `LINGER` once its handle is gone); charge
    // it to the pool if kept buffers must count against the cap.
    orphan: bool,
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
    shut: bool,
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
    /// The driver's part: on readiness, does what the side holds and wakes
    /// the tasks that wait on any of it.
    pub(super) fn serve(&mut self, fd: RawFd, kind: Kind, edge: bool, after: &mut After) {
        if edge {
            self.blocked = false;
        }
        if self.push_fd(fd, kind, after) {
            after.wake(&mut self.waiters);
        }
    }

    /// The driver's part for what the side holds: its calls on the
    /// descriptor, or its next operation on the ring. Returns whether
    /// anything changed at once.
    fn push(&mut self, io: Io<'_>, kind: Kind, after: &mut After) -> bool {
        match io {
            #[cfg(readiness)]
            Io::Fd(fd) => self.push_fd(fd, kind, after),
            #[cfg(completion)]
            Io::Ops(route) => {
                self.submit_ops(route, kind, after);
                false
            }
            #[cfg(not(completion))]
            Io::None(never, _) => match never {},
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
    /// lent buffer, sends a queued datagram. Returns whether anything
    /// changed.
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
        if !self.blocked
            && let Some((datagram, target)) = self.queued.as_ref()
        {
            match sys::ready(send_datagram(fd, datagram.filled(), target.as_dest())) {
                None => self.blocked = true,
                Some(result) => {
                    if let Err(err) = result {
                        self.error = Some(err);
                    }
                    if let Some((done, _)) = self.queued.take() {
                        after.drop_buf(done);
                    }
                    progress = true;
                }
            }
        }
        progress
    }

    #[cfg(readiness)]
    /// Writes the lent buffer out, after the write-behind. Returns whether
    /// anything changed.
    fn push_lent(&mut self, fd: RawFd, kind: Kind, after: &mut After) -> bool {
        let Some(lent) = self.lent.as_mut() else {
            return false;
        };
        let mut progress = false;
        if let Some(failure) = self.failed {
            // The write-behind before it failed: none of it went out.
            lent.done = Some(Err(failure.error()));
        }
        while lent.done.is_none() && !self.blocked {
            let rest = lent.buf.get(lent.sent..).unwrap_or_default();
            match sys::ready(write_bytes(fd, kind, rest)) {
                None => self.blocked = true,
                Some(Ok(0)) => lent.done = Some(Err(io::ErrorKind::WriteZero.into())),
                Some(Ok(n)) => {
                    lent.sent += n;
                    progress = true;
                    if lent.sent >= lent.buf.len() {
                        lent.done = Some(Ok(lent.buf.len()));
                    }
                }
                Some(Err(err)) => lent.done = Some(Err(err)),
            }
        }
        if lent.done.is_none() {
            return progress;
        }
        self.lent_finished(after);
        true
    }

    /// The lent buffer's write ended: its task is woken, and one whose task
    /// went is dropped.
    fn lent_finished(&mut self, after: &mut After) {
        let Some(lent) = self.lent.as_mut() else {
            return;
        };
        if let Some(w) = lent.waker.take() {
            after.wake_one(w);
        }
        if lent.orphan
            && let Some(Lent { done, .. }) = self.lent.take()
            && let Some(Err(err)) = done
        {
            // Nobody waits for this write: its error is the write-behind's.
            self.failed = Some(Failure::of(&err));
        }
    }

    /// A write of all of a task's own buffer (`held`; `None` while it is
    /// lent): lent to the side, and written out by the driver at once if
    /// the socket may take it and on its edges if not. Ready once all of it
    /// went out (or failed), with the buffer back in `held`.
    fn write_owned(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        held: &mut Option<Vec<u8>>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if let Some(buf) = held.as_ref() {
            if let Some(failure) = self.failed {
                return Poll::Ready(Err(failure.error()));
            }
            if let Some(err) = self.error.take() {
                return Poll::Ready(Err(err));
            }
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            if self.lent.is_some() {
                // Another buffer is lent: this one goes after it.
                self.waiters.register(waker);
                return Poll::Pending;
            }
            self.lent = held.take().map(|buf| Lent {
                buf,
                sent: 0,
                done: None,
                waker: None,
                out: false,
                orphan: false,
            });
        }
        // The task polls, so the driver's write needs no wake for it.
        let old = self.lent.as_mut().and_then(|l| l.waker.take());
        self.push(io, kind, after);
        let Some(lent) = self.lent.as_mut() else {
            return Poll::Ready(Err(io::Error::other("the write lost its buffer")));
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
            return Poll::Ready(Err(io::Error::other("the write lost its buffer")));
        };
        *held = Some(buf);
        // Writes that waited for the lent buffer go on.
        after.wake(&mut self.waiters);
        Poll::Ready(done.unwrap_or(Ok(0)))
    }

    /// The task of the lent buffer went away: a write none of which went
    /// out is withdrawn, one that started is left to the driver to finish,
    /// and the error of one that ended is kept as the write-behind's.
    pub(super) fn abandon_lent(&mut self, after: &mut After) {
        let Some(lent) = self.lent.as_mut() else {
            return;
        };
        if let Some(w) = lent.waker.take() {
            after.forget_one(w);
        }
        // Out with a ring operation, some of it may be going out: its
        // completion withdraws it if none did.
        if lent.done.is_none() && (lent.sent > 0 || lent.out) {
            lent.orphan = true;
            return;
        }
        if let Some(Lent {
            done: Some(Err(err)),
            ..
        }) = self.lent.take()
        {
            self.failed = Some(Failure::of(&err));
        }
        after.wake(&mut self.waiters);
    }

    /// The lent buffer back, as it is, for a poll that ends in an error.
    fn reclaim(&mut self, after: &mut After) -> Option<Vec<u8>> {
        if self.lent.as_ref().is_some_and(|l| l.out) {
            // Out with a ring operation, it cannot come back now: the
            // write goes on as if its task went away.
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
            None => match self.buffer(len.max(BEHIND_MIN), false, after) {
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
            // the write waits for the driver to make some.
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

    /// Flushes, then shuts a stream's writing half of `fd` down (with
    /// nothing out on the ring by then, the driver's call on it at once).
    fn close(
        &mut self,
        io: Io<'_>,
        fd: crate::io::raw::Raw,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<()>> {
        match self.flush(io, kind, waker, after) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        if kind.streams() && !self.shut {
            self.shut = true;
            return Poll::Ready(sys::shutdown_write(fd).inspect_err(crate::rt::counters::io_error));
        }
        Poll::Ready(Ok(()))
    }

    /// A datagram to `dest`: copied and queued, and sent by the driver at
    /// once if the socket may take it.
    fn send(
        &mut self,
        io: Io<'_>,
        kind: Kind,
        bytes: &[u8],
        dest: Dest<'_>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        if self.queued.is_some() {
            self.waiters.register(waker);
            return Poll::Pending;
        }
        let Some(mut datagram) = self.buffer(bytes.len(), true, after) else {
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
            after.register = true;
        }
        Poll::Ready(Ok(bytes.len()))
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
            return Poll::Ready(result);
        }
        if !self.connect_started {
            self.connect_started = true;
            match io {
                #[cfg(readiness)]
                Io::Fd(fd) => {
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
                            crate::rt::counters::io_error(&err);
                            return Poll::Ready(Err(err));
                        }
                    }
                }
                #[cfg(completion)]
                Io::Ops(route) => {
                    self.dial = Some(match peer {
                        Peer::Inet(addr) => super::ops::Dial::Inet(addr),
                        #[cfg(unix)]
                        Peer::Unix(addr) => super::ops::Dial::Unix(*addr),
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
        self.shut = false;
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

impl Source {
    /// Writes `bytes` to a stream: copied into the write-behind and written
    /// out by the driver.
    pub(crate) fn poll_write(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored(cx, &[io::IoSlice::new(bytes)])
    }

    /// Writes the bytes of `bufs`, gathered in order, as
    /// [`poll_write`](Self::poll_write) writes one slice.
    pub(crate) fn poll_write_vectored(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        crate::coop::poll_budget(cx, |cx| {
            self.poll_writing(cx, |st, io, waker, after| {
                st.write(io, kind, bufs, waker, after)
            })
        })
    }

    /// Polls the write side with `step`, given the way this thread reaches
    /// the descriptor.
    fn poll_writing<R>(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        mut step: impl FnMut(&mut WriteState, Io<'_>, &Waker, &mut After) -> Poll<io::Result<R>>,
    ) -> Poll<io::Result<R>> {
        let mut after = After::default();
        let polled = self.with_io(|io| {
            self.write
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

    /// Writes all of `buf` and gives it back with the write's result. The
    /// buffer is lent to the side until the kernel took the last byte, so
    /// the bytes go out from it with no copy.
    pub(crate) async fn write_owned(
        self: &Arc<Self>,
        buf: Vec<u8>,
    ) -> (io::Result<usize>, Vec<u8>) {
        let mut op = WriteOwned {
            source: self,
            buf: Some(buf),
            lent: false,
        };
        let result = poll_fn(|cx| op.poll(cx)).await;
        (result, op.buf.take().unwrap_or_default())
    }

    fn poll_write_owned(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        held: &mut Option<Vec<u8>>,
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        let poll = self.poll_writing(cx, |st, io, waker, after| {
            st.write_owned(io, kind, held, waker, after)
        });
        if poll.is_ready() && held.is_none() {
            // The poll failed with the buffer lent: it comes back as it is.
            // If the side is held now, the poll waits and starts over.
            let mut after = After::default();
            let back = self
                .write
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

    /// Completes once the write-behind is out (or reports why it stopped).
    pub(crate) fn poll_flush(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| st.flush(io, kind, waker, after))
    }

    /// Flushes, then shuts a stream socket's writing half down.
    pub(crate) fn poll_close(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| {
            st.close(io, fd, kind, waker, after)
        })
    }

    /// Sends a datagram to `dest`: copied, queued and sent by the driver.
    pub(crate) fn poll_send(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
        dest: Dest<'_>,
    ) -> Poll<io::Result<usize>> {
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| {
            st.send(io, kind, bytes, dest, waker, after)
        })
    }

    /// Completes once the kernel took every byte and datagram accepted (or
    /// a failure ended them).
    pub(crate) fn poll_drain(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<()> {
        let kind = self.kind;
        match self.poll_writing(cx, |st, io, waker, after| {
            st.drain(io, kind, waker, after).map(Ok)
        }) {
            Poll::Ready(_) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Connects the socket to `peer`: the driver starts the connect on the
    /// first poll and sees it end. Polled again after it completes, it
    /// completes at once.
    pub(crate) fn poll_connect(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        peer: Peer<'_>,
    ) -> Poll<io::Result<()>> {
        let kind = self.kind;
        self.poll_writing(cx, |st, io, waker, after| {
            st.connect(io, kind, peer, waker, after)
        })
    }
}

/// A write whose buffer may be lent to the side. Dropped while it is, the
/// side withdraws the write if none of it went out and finishes it if some
/// did.
struct WriteOwned<'a> {
    source: &'a Arc<Source>,
    buf: Option<Vec<u8>>,
    lent: bool,
}

impl WriteOwned<'_> {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let poll = self.source.poll_write_owned(cx, &mut self.buf);
        self.lent = self.buf.is_none();
        poll
    }
}

impl Drop for WriteOwned<'_> {
    fn drop(&mut self) {
        if self.lent {
            let mut after = After::default();
            self.source
                .write
                .defer(super::write_work::ABANDON, &mut after, &self.source.hooks());
            self.source.settle(after);
        }
    }
}
