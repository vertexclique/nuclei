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
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::Arc;
use std::task::Context;

use super::{Failure, Kind, Source, Target};
use crate::buf::{PoolBuf, with_pool};
use crate::io::side::{After, Wakers};
use crate::io::sys;

/// The least a write-behind buffer holds, so a run of small writes fills
/// one buffer rather than waiting on each.
const BEHIND_MIN: usize = 16 * 1024;

/// Where a datagram goes, borrowed for one send.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Dest<'a> {
    Connected,
    Inet(SocketAddr),
    Unix(&'a Path),
}

impl Dest<'_> {
    fn to_owned(self) -> Target {
        match self {
            Dest::Connected => Target::Connected,
            Dest::Inet(addr) => Target::Inet(addr),
            Dest::Unix(path) => Target::Unix(path.to_path_buf()),
        }
    }
}

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
    Unix(&'a Path),
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
    /// The task went away after some of the buffer went out: the driver
    /// writes the rest, then drops it.
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
}

/// Writes bytes to a stream.
fn write_bytes(fd: RawFd, kind: Kind, bytes: &[u8]) -> io::Result<usize> {
    match kind {
        Kind::Bytes | Kind::File => sys::write(fd, bytes),
        _ => sys::send(fd, bytes),
    }
}

/// Sends a datagram.
fn send_datagram(fd: RawFd, bytes: &[u8], dest: Dest<'_>) -> io::Result<usize> {
    match dest {
        Dest::Connected => sys::send(fd, bytes),
        Dest::Inet(addr) => sys::send_to_udp(fd, bytes, addr),
        Dest::Unix(path) => sys::send_to_unix(fd, bytes, path),
    }
}

impl WriteState {
    #[cfg(test)]
    pub(super) fn has_behind(&self) -> bool {
        self.behind.is_some()
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

    /// The driver's part: on readiness, does what the side holds and wakes
    /// the tasks that wait on any of it.
    pub(super) fn serve(&mut self, fd: RawFd, kind: Kind, edge: bool, after: &mut After) {
        if edge {
            self.blocked = false;
        }
        if self.push(fd, kind, after) {
            after.wake(&mut self.waiters);
        }
    }

    /// The driver's calls on the descriptor, while the socket takes them:
    /// ends a connect in progress, writes the write-behind out and then the
    /// lent buffer, sends a queued datagram. Returns whether anything
    /// changed.
    fn push(&mut self, fd: RawFd, kind: Kind, after: &mut After) -> bool {
        if self.blocked {
            return false;
        }
        let mut progress = false;
        if self.connecting {
            self.connecting = false;
            self.connected = Some(match sys::take_error(fd) {
                Ok(None) => Ok(()),
                Ok(Some(err)) | Err(err) => Err(err),
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
        true
    }

    /// A write of all of a task's own buffer (`held`; `None` while it is
    /// lent): lent to the side, and written out by the driver at once if
    /// the socket may take it and on its edges if not. Ready once all of it
    /// went out (or failed), with the buffer back in `held`.
    fn write_owned(
        &mut self,
        fd: RawFd,
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
                orphan: false,
            });
        }
        // The task polls, so the driver's write needs no wake for it.
        let old = self.lent.as_mut().and_then(|l| l.waker.take());
        self.push(fd, kind, after);
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
    fn abandon_lent(&mut self, after: &mut After) {
        let Some(lent) = self.lent.as_mut() else {
            return;
        };
        if let Some(w) = lent.waker.take() {
            after.forget_one(w);
        }
        if lent.done.is_none() && lent.sent > 0 {
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
        fd: RawFd,
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
        self.push(fd, kind, after);
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
        fd: RawFd,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<()>> {
        self.push(fd, kind, after);
        if let Some(failure) = self.failed.take() {
            return Poll::Ready(Err(failure.error()));
        }
        if self.behind.is_some() || self.lent_waits() {
            self.waiters.register(waker);
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }

    fn close(
        &mut self,
        fd: RawFd,
        kind: Kind,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<()>> {
        match self.flush(fd, kind, waker, after) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        if matches!(kind, Kind::Stream | Kind::Tcp) && !self.shut {
            self.shut = true;
            return Poll::Ready(sys::shutdown_write(fd));
        }
        Poll::Ready(Ok(()))
    }

    /// A datagram to `dest`: copied and queued, and sent by the driver at
    /// once if the socket may take it.
    fn send(
        &mut self,
        fd: RawFd,
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
        self.push(fd, kind, after);
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
    fn has_unsent(&self) -> bool {
        self.failed.is_none()
            && (self.behind.is_some() || self.lent_waits() || self.queued.is_some())
    }

    fn drain(&mut self, fd: RawFd, kind: Kind, waker: &Waker, after: &mut After) -> Poll<()> {
        self.push(fd, kind, after);
        if self.has_unsent() {
            self.waiters.register(waker);
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    /// A connect to `peer`: the driver starts it on the first poll, and sees
    /// it end on the socket's first edge.
    fn connect(
        &mut self,
        fd: RawFd,
        peer: Peer<'_>,
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<()>> {
        if let Some(result) = self.connected.take() {
            return Poll::Ready(result);
        }
        if !self.connect_started {
            self.connect_started = true;
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
                Err(err) => return Poll::Ready(Err(err)),
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
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.write.poll(
            &mut after,
            |st, after| st.write(fd, kind, bufs, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
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
        let poll = match self.fd() {
            Ok(fd) => {
                let kind = self.kind;
                let mut after = After::default();
                let poll = self.write.poll(
                    &mut after,
                    |st, after| st.write_owned(fd, kind, held, cx.waker(), after),
                    |st, after| st.serve(fd, kind, true, after),
                );
                self.finish_poll(after, poll)
            }
            Err(err) => Poll::Ready(Err(err)),
        };
        if poll.is_ready() && held.is_none() {
            let mut after = After::default();
            *held = self.write.run(
                &mut after,
                |st, after| st.reclaim(after),
                self.serve_write(),
            );
            self.settle(after);
        }
        poll
    }

    /// Completes once the write-behind is out (or reports why it stopped).
    pub(crate) fn poll_flush(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.write.poll(
            &mut after,
            |st, after| st.flush(fd, kind, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
    }

    /// Flushes, then shuts a stream socket's writing half down.
    pub(crate) fn poll_close(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.write.poll(
            &mut after,
            |st, after| st.close(fd, kind, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
    }

    /// Sends a datagram to `dest`: copied, queued and sent by the driver.
    pub(crate) fn poll_send(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
        dest: Dest<'_>,
    ) -> Poll<io::Result<usize>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.write.poll(
            &mut after,
            |st, after| st.send(fd, kind, bytes, dest, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
    }

    /// Whether the write side holds bytes or a datagram the kernel has not
    /// taken yet.
    pub(crate) fn has_unsent(&self) -> bool {
        self.write.hold().get().has_unsent()
    }

    /// Completes once the kernel took every byte and datagram accepted (or
    /// a failure ended them).
    pub(crate) fn poll_drain(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<()> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(_) => return Poll::Ready(()),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.write.poll(
            &mut after,
            |st, after| st.drain(fd, kind, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        match self.finish_poll(after, poll.map(Ok)) {
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
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.write.poll(
            &mut after,
            |st, after| st.connect(fd, peer, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
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
            self.source.write.run(
                &mut after,
                |st, after| st.abandon_lent(after),
                self.source.serve_write(),
            );
            self.source.settle(after);
        }
    }
}
