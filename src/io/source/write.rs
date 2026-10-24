//! The write side of a source: writes, flushes, shutdown, datagram sends
//! and connects.
//!
//! A write that would block is accepted at once: its bytes are copied into
//! a buffer of the pool (the write-behind, one per source), and the driver
//! writes them out as the socket takes more. Later writes add to that
//! buffer while it has room and wait while it is full, so bytes go out in
//! the order they were written. A write-behind that fails keeps its error:
//! every write fails with it until a flush reports it. A datagram that
//! would block is queued the same way, one at a time, and an error sending
//! it is reported by the next send.

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

#[derive(Default)]
pub(super) struct WriteState {
    /// The last call would have blocked, and no edge came since.
    blocked: bool,
    waiters: Wakers,
    /// A write found the pool empty; the source waits for a buffer.
    starved: bool,
    behind: Option<PoolBuf>,
    queued: Option<(PoolBuf, Target)>,
    /// Why the write-behind stopped.
    failed: Option<Failure>,
    /// An error the next call reports once.
    error: Option<io::Error>,
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

    fn buffer(&mut self, len: usize, whole: bool, after: &mut After) -> Option<PoolBuf> {
        let buf =
            with_pool(|pool| pool.and_then(|p| if whole { p.get_whole(len) } else { p.get(len) }));
        if buf.is_none() && !self.starved {
            self.starved = true;
            after.starved = true;
        }
        buf
    }

    /// The driver's part: on readiness, finishes a connect, writes the
    /// write-behind out and sends a queued datagram; wakes the tasks that
    /// wait on any of it.
    pub(super) fn serve(&mut self, fd: RawFd, kind: Kind, edge: bool, after: &mut After) {
        if edge {
            self.blocked = false;
        }
        if self.blocked {
            return;
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
        if self.starved && !self.blocked {
            // The socket takes bytes again: a write that found no buffer
            // goes straight to it.
            self.starved = false;
            progress = true;
        }
        if progress {
            after.wake(&mut self.waiters);
        }
    }

    fn write(
        &mut self,
        fd: RawFd,
        kind: Kind,
        bytes: &[u8],
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        if let Some(failure) = self.failed {
            return Poll::Ready(Err(failure.error()));
        }
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if let Some(behind) = self.behind.as_mut() {
            let n = behind.append(bytes);
            if n > 0 {
                return Poll::Ready(Ok(n));
            }
            self.waiters.register(waker);
            return Poll::Pending;
        }
        if !self.blocked {
            match sys::ready(write_bytes(fd, kind, bytes)) {
                Some(result) => return Poll::Ready(result),
                None => self.blocked = true,
            }
        }
        match self.buffer(bytes.len().max(BEHIND_MIN), false, after) {
            Some(mut behind) => {
                let n = behind.fill_from(bytes);
                self.behind = Some(behind);
                after.register = true;
                Poll::Ready(Ok(n))
            }
            None => {
                self.waiters.register(waker);
                Poll::Pending
            }
        }
    }

    fn flush(&mut self, waker: &Waker) -> Poll<io::Result<()>> {
        if let Some(failure) = self.failed.take() {
            return Poll::Ready(Err(failure.error()));
        }
        if self.behind.is_some() {
            self.waiters.register(waker);
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }

    fn close(&mut self, fd: RawFd, kind: Kind, waker: &Waker) -> Poll<io::Result<()>> {
        match self.flush(waker) {
            Poll::Ready(Ok(())) => {}
            other => return other,
        }
        if kind == Kind::Stream && !self.shut {
            self.shut = true;
            return Poll::Ready(sys::shutdown_write(fd));
        }
        Poll::Ready(Ok(()))
    }

    fn send(
        &mut self,
        fd: RawFd,
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
        if !self.blocked {
            match sys::ready(send_datagram(fd, bytes, dest)) {
                Some(result) => return Poll::Ready(result),
                None => self.blocked = true,
            }
        }
        match self.buffer(bytes.len(), true, after) {
            Some(mut datagram) => {
                datagram.fill_from(bytes);
                self.queued = Some((datagram, dest.to_owned()));
                after.register = true;
                Poll::Ready(Ok(bytes.len()))
            }
            None => {
                self.waiters.register(waker);
                Poll::Pending
            }
        }
    }

    /// Bytes or a datagram accepted and not handed to the kernel yet, that
    /// the driver can still deliver.
    fn has_unsent(&self) -> bool {
        self.failed.is_none() && (self.behind.is_some() || self.queued.is_some())
    }

    fn drain(&mut self, waker: &Waker) -> Poll<()> {
        if self.has_unsent() {
            self.waiters.register(waker);
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    fn connect(&mut self, waker: &Waker) -> Poll<io::Result<()>> {
        if let Some(result) = self.connected.take() {
            return Poll::Ready(result);
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
        after.wake(&mut self.waiters);
    }

    /// A buffer came back to the pool a write found empty.
    pub(super) fn unstarve(&mut self, after: &mut After) {
        if self.starved {
            self.starved = false;
            after.wake(&mut self.waiters);
        }
    }

    pub(super) fn forget_waiters(&mut self, after: &mut After) {
        self.starved = false;
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
        self.failed = None;
        self.error = None;
        self.connecting = false;
        self.connected = None;
        self.shut = false;
        self.blocked = false;
    }
}

impl Source {
    /// Writes `bytes` to a stream: at once if the socket takes them, else
    /// into the write-behind.
    pub(crate) fn poll_write(
        self: &Arc<Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.write.poll(
            &mut after,
            |st, after| st.write(fd, kind, bytes, cx.waker(), after),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
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
            |st, _| st.flush(cx.waker()),
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
            |st, _| st.close(fd, kind, cx.waker()),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
    }

    /// Sends a datagram to `dest`: at once if the socket takes it, else
    /// queued for the driver.
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
            |st, after| st.send(fd, bytes, dest, cx.waker(), after),
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
            |st, _| st.drain(cx.waker()),
            |st, after| st.serve(fd, kind, true, after),
        );
        match self.finish_poll(after, poll.map(Ok)) {
            Poll::Ready(_) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }
    }

    /// A non-blocking connect is in progress: the driver sees it end.
    pub(crate) fn begin_connect(self: &Arc<Self>) {
        let mut after = After::default();
        self.write.run(
            &mut after,
            |st, _| {
                st.connecting = true;
                st.connected = None;
            },
            self.serve_write(),
        );
        self.settle(after);
    }

    /// Completes when the connect begun with
    /// [`begin_connect`](Self::begin_connect) ended, with its outcome.
    pub(crate) fn poll_connect(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fd = match self.fd() {
            Ok(fd) => fd,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let kind = self.kind;
        let mut after = After::default();
        let poll = self.write.poll(
            &mut after,
            |st, _| st.connect(cx.waker()),
            |st, after| st.serve(fd, kind, true, after),
        );
        self.finish_poll(after, poll)
    }
}
