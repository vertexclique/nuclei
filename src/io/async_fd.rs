//! [`AsyncFd`]: a caller's own descriptor, driven by readiness. The shard's
//! driver says when it may be ready (an edge on epoll and kqueue, a poll on
//! io_uring), and the caller makes its own calls on it, clearing the
//! readiness when one would block. It is for descriptors the caller
//! manages (an eventfd, a device, a socket of a kind nuclei does not wrap);
//! nuclei's sockets and files are completed by the driver instead.
//!
//! The descriptor must be in non-blocking mode; its mode is left as it is.
//! Readiness may be spurious (the side starts ready, and a guard says what
//! may be ready, not what is): a call that would block answers
//! `WouldBlock`, and clearing the guard's readiness makes the next wait
//! last until the driver says so again. A clear that comes after the
//! driver's next word is not lost: each guard remembers what it saw
//! (proofs/tla/asyncfd).
//!
//! ```
//! use std::os::unix::net::UnixStream;
//! use std::io::{Read, Write};
//! use nuclei::io::unix::AsyncFd;
//!
//! # nuclei::block_on(async {
//! let (a, mut b) = UnixStream::pair()?;
//! a.set_nonblocking(true)?;
//! let fd = AsyncFd::new(a)?;
//! b.write_all(b"ping")?;
//! let mut buf = [0u8; 4];
//! let n = fd.async_io(nuclei::io::Interest::READABLE, |mut a| a.read(&mut buf)).await?;
//! assert_eq!(&buf[..n], b"ping");
//! # std::io::Result::Ok(())
//! # }).unwrap();
//! ```

use core::fmt;
use core::future::poll_fn;
use core::task::{Context, Poll, ready};
use std::io;
use std::os::fd::AsFd;

use super::caller::{self, Marks};
use super::source::Kind;
use super::{Handle, Interest, Ready};

/// A caller's own descriptor, driven by readiness.
pub struct AsyncFd<T: AsFd> {
    handle: Handle<T>,
    interest: Interest,
}

/// What a readiness wait of an [`AsyncFd`] found: the readiness, and the
/// edge counts it is of, to clear with.
pub struct AsyncFdReadyGuard<'a, T: AsFd> {
    fd: &'a AsyncFd<T>,
    marks: Marks,
}

/// As [`AsyncFdReadyGuard`], with the descriptor borrowed mutably.
pub struct AsyncFdReadyMutGuard<'a, T: AsFd> {
    fd: &'a mut AsyncFd<T>,
    marks: Marks,
}

/// The call of a guard's `try_io` would block: the guard's readiness was
/// cleared.
#[derive(Debug)]
pub struct TryIoError(());

impl<T: AsFd> AsyncFd<T> {
    /// Drives `inner` by readiness for reading and writing both.
    ///
    /// # Errors
    ///
    /// Never today: the descriptor registers with the shard that first
    /// waits on it, and a poller that refuses it (a regular file, on epoll)
    /// says so at that wait.
    pub fn new(inner: T) -> io::Result<Self> {
        Self::with_interest(inner, Interest::READABLE | Interest::WRITABLE)
    }

    /// Drives `inner` by readiness for what `interest` names.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn with_interest(inner: T, interest: Interest) -> io::Result<Self> {
        Ok(Self {
            handle: Handle::from_nonblocking(inner, Kind::Fd),
            interest,
        })
    }

    /// The descriptor.
    pub fn get_ref(&self) -> &T {
        self.handle.get_ref()
    }

    /// The descriptor, to change.
    pub fn get_mut(&mut self) -> &mut T {
        self.handle.get_mut()
    }

    /// The descriptor back, no longer watched.
    pub fn into_inner(self) -> T {
        self.handle.into_inner()
    }

    /// Polls until the descriptor may be readable.
    ///
    /// # Errors
    ///
    /// When the descriptor cannot be watched.
    pub fn poll_read_ready<'a>(
        &'a self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<AsyncFdReadyGuard<'a, T>>> {
        let tick = ready!(self.handle.source().poll_fd_read_ready(cx))?;
        Poll::Ready(Ok(AsyncFdReadyGuard {
            fd: self,
            marks: Marks {
                ready: Ready::READABLE,
                read: Some(tick),
                write: None,
            },
        }))
    }

    /// Polls until the descriptor may be writable.
    ///
    /// # Errors
    ///
    /// When the descriptor cannot be watched.
    pub fn poll_write_ready<'a>(
        &'a self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<AsyncFdReadyGuard<'a, T>>> {
        let tick = ready!(self.handle.source().poll_fd_write_ready(cx))?;
        Poll::Ready(Ok(AsyncFdReadyGuard {
            fd: self,
            marks: Marks {
                ready: Ready::WRITABLE,
                read: None,
                write: Some(tick),
            },
        }))
    }

    /// The sides of `interest` the descriptor is watched for; none is an
    /// error, since a wait for them could never end.
    fn sides(&self, interest: Interest) -> io::Result<(bool, bool)> {
        let (read, write) = caller::sides(interest);
        let (read, write) = (
            read && self.interest.is_readable(),
            write && self.interest.is_writable(),
        );
        if !read && !write {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the descriptor is not watched for that readiness",
            ));
        }
        Ok((read, write))
    }

    /// What of `interest` may be ready now, waiting until some is.
    fn poll_marks(&self, cx: &mut Context<'_>, interest: Interest) -> Poll<io::Result<Marks>> {
        let (read, write) = self.sides(interest)?;
        caller::poll_marks(self.handle.source(), cx, read, write)
    }

    /// Waits until the descriptor may be readable.
    ///
    /// # Errors
    ///
    /// When the descriptor cannot be watched.
    pub async fn readable(&self) -> io::Result<AsyncFdReadyGuard<'_, T>> {
        self.ready(Interest::READABLE).await
    }

    /// Waits until the descriptor may be writable.
    ///
    /// # Errors
    ///
    /// When the descriptor cannot be watched.
    pub async fn writable(&self) -> io::Result<AsyncFdReadyGuard<'_, T>> {
        self.ready(Interest::WRITABLE).await
    }

    /// Waits until some of `interest` may be ready.
    ///
    /// # Errors
    ///
    /// When the descriptor cannot be watched, and `InvalidInput` when it is
    /// not watched for any of `interest`.
    pub async fn ready(&self, interest: Interest) -> io::Result<AsyncFdReadyGuard<'_, T>> {
        let marks = poll_fn(|cx| self.poll_marks(cx, interest)).await?;
        Ok(AsyncFdReadyGuard { fd: self, marks })
    }

    /// As [`readable`](Self::readable), with the descriptor borrowed
    /// mutably.
    ///
    /// # Errors
    ///
    /// When the descriptor cannot be watched.
    pub async fn readable_mut(&mut self) -> io::Result<AsyncFdReadyMutGuard<'_, T>> {
        self.ready_mut(Interest::READABLE).await
    }

    /// As [`writable`](Self::writable), with the descriptor borrowed
    /// mutably.
    ///
    /// # Errors
    ///
    /// When the descriptor cannot be watched.
    pub async fn writable_mut(&mut self) -> io::Result<AsyncFdReadyMutGuard<'_, T>> {
        self.ready_mut(Interest::WRITABLE).await
    }

    /// As [`ready`](Self::ready), with the descriptor borrowed mutably.
    ///
    /// # Errors
    ///
    /// When the descriptor cannot be watched.
    pub async fn ready_mut(
        &mut self,
        interest: Interest,
    ) -> io::Result<AsyncFdReadyMutGuard<'_, T>> {
        let marks = poll_fn(|cx| self.poll_marks(cx, interest)).await?;
        Ok(AsyncFdReadyMutGuard { fd: self, marks })
    }

    /// Runs `f` on the descriptor each time some of `interest` may be ready,
    /// until it answers other than `WouldBlock`, clearing the readiness
    /// each time it does.
    ///
    /// # Errors
    ///
    /// What `f` answers, when the descriptor cannot be watched, and
    /// `InvalidInput` when it is not watched for any of `interest`.
    pub async fn async_io<R>(
        &self,
        interest: Interest,
        mut f: impl FnMut(&T) -> io::Result<R>,
    ) -> io::Result<R> {
        let sides = self.sides(interest)?;
        caller::async_io(self.handle.source(), sides, || f(self.get_ref())).await
    }

    /// Runs `f` on the descriptor now if some of `interest` may be ready,
    /// clearing that readiness if it answers `WouldBlock`.
    ///
    /// # Errors
    ///
    /// `WouldBlock` when none of `interest` may be ready, `InvalidInput`
    /// when the descriptor is not watched for any of it, and what `f`
    /// answers.
    pub fn try_io<R>(
        &self,
        interest: Interest,
        f: impl FnOnce(&T) -> io::Result<R>,
    ) -> io::Result<R> {
        caller::try_io(self.handle.source(), self.sides(interest)?, || {
            f(self.get_ref())
        })
    }

    fn clear(&self, marks: Marks) {
        caller::clear(self.handle.source(), marks);
    }
}

macro_rules! guard {
    ($guard:ident) => {
        impl<T: AsFd> $guard<'_, T> {
            /// What may be ready.
            pub fn ready(&self) -> Ready {
                self.marks.ready
            }

            /// The descriptor would block: the next wait lasts until the
            /// driver says it may be ready again.
            pub fn clear_ready(&mut self) {
                self.fd.clear(self.marks);
                self.marks.ready = Ready::EMPTY;
            }

            /// Clears what of `ready` this guard holds.
            pub fn clear_ready_matching(&mut self, ready: Ready) {
                let mut marks = self.marks;
                if !ready.is_readable() {
                    marks.read = None;
                }
                if !ready.is_writable() {
                    marks.write = None;
                }
                self.fd.clear(marks);
                if ready.is_readable() {
                    self.marks.read = None;
                }
                if ready.is_writable() {
                    self.marks.write = None;
                }
                self.marks.ready = Ready::EMPTY;
                if self.marks.read.is_some() {
                    self.marks.ready = self.marks.ready | Ready::READABLE;
                }
                if self.marks.write.is_some() {
                    self.marks.ready = self.marks.ready | Ready::WRITABLE;
                }
            }

            /// Keeps the readiness: the next wait may answer at once.
            pub fn retain_ready(&mut self) {}

            /// The descriptor.
            pub fn get_inner(&self) -> &T {
                self.fd.get_ref()
            }
        }

        impl<T: AsFd> fmt::Debug for $guard<'_, T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($guard))
                    .field("ready", &self.marks.ready)
                    .finish_non_exhaustive()
            }
        }
    };
}

guard!(AsyncFdReadyGuard);
guard!(AsyncFdReadyMutGuard);

impl<'a, T: AsFd> AsyncFdReadyGuard<'a, T> {
    /// Runs `f`; if it answers `WouldBlock`, the readiness is cleared and
    /// `Err(TryIoError)` comes back, else `Ok` with what it answered.
    ///
    /// # Errors
    ///
    /// [`TryIoError`] when `f` would block.
    pub fn try_io<R>(
        &mut self,
        f: impl FnOnce(&'a AsyncFd<T>) -> io::Result<R>,
    ) -> Result<io::Result<R>, TryIoError> {
        match f(self.fd) {
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                self.clear_ready();
                Err(TryIoError(()))
            }
            result => Ok(result),
        }
    }

    /// The descriptor's `AsyncFd`.
    pub fn get_ref(&self) -> &'a AsyncFd<T> {
        self.fd
    }
}

impl<T: AsFd> AsyncFdReadyMutGuard<'_, T> {
    /// Runs `f`; if it answers `WouldBlock`, the readiness is cleared and
    /// `Err(TryIoError)` comes back, else `Ok` with what it answered.
    ///
    /// # Errors
    ///
    /// [`TryIoError`] when `f` would block.
    pub fn try_io<R>(
        &mut self,
        f: impl FnOnce(&mut AsyncFd<T>) -> io::Result<R>,
    ) -> Result<io::Result<R>, TryIoError> {
        match f(self.fd) {
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                self.clear_ready();
                Err(TryIoError(()))
            }
            result => Ok(result),
        }
    }

    /// The descriptor's `AsyncFd`.
    pub fn get_ref(&self) -> &AsyncFd<T> {
        self.fd
    }

    /// The descriptor's `AsyncFd`, mutably.
    pub fn get_mut(&mut self) -> &mut AsyncFd<T> {
        self.fd
    }

    /// The descriptor, mutably.
    pub fn get_inner_mut(&mut self) -> &mut T {
        self.fd.get_mut()
    }
}

impl<T: AsFd + fmt::Debug> fmt::Debug for AsyncFd<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncFd")
            .field("inner", self.get_ref())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for TryIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the call would block")
    }
}

impl std::error::Error for TryIoError {}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    use super::*;

    /// Whether this thread's shard, which serves the test's descriptor, runs a ring: other tests
    /// may run shards on the poller meanwhile.
    fn on_ring() -> bool {
        crate::Proactor::backend() == crate::IoBackend::IoUring
    }

    /// An edge that comes between a call that would block and the clear of
    /// its guard keeps the side ready: the clear is of the look before the
    /// call (proofs/tla/asyncfd, the no_tick mutation). On a ring the side
    /// counts only the polls it arms, each after a clear, so the clear
    /// there comes first and the poll it arms finds the byte.
    #[test]
    fn an_edge_after_the_call_survives_the_clear() {
        crate::block_on(async {
            let (a, mut b) = UnixStream::pair().unwrap();
            a.set_nonblocking(true).unwrap();
            let fd = AsyncFd::with_interest(a, Interest::READABLE).unwrap();
            let mut buf = [0u8; 8];
            // A wait that waits registers the descriptor with the driver.
            let mut guard = fd.readable().await.unwrap();
            assert!(guard.try_io(|fd| fd.get_ref().read(&mut buf)).is_err());
            b.write_all(b"r").unwrap();
            let read = fd.async_io(Interest::READABLE, |mut a| a.read(&mut buf));
            assert_eq!(read.await.unwrap(), 1);
            let mut first = fd.readable().await.unwrap();
            let err = fd.get_ref().read(&mut buf).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
            b.write_all(b"x").unwrap();
            if on_ring() {
                first.clear_ready();
                let n = fd.async_io(Interest::READABLE, |mut a| a.read(&mut buf));
                assert_eq!(n.await.unwrap(), 1);
                return;
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                crate::task::yield_now().await;
                let look = fd.readable().await.unwrap();
                if look.marks.read != first.marks.read {
                    break;
                }
                assert!(Instant::now() < deadline, "the driver never saw the edge");
            }
            first.clear_ready();
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(
                fd.poll_read_ready(&mut cx).is_ready(),
                "the edge after the call was lost to the clear"
            );
            assert_eq!(fd.get_ref().read(&mut buf).unwrap(), 1);
        });
    }

    /// Bytes written into `fd` until a write would block.
    fn fill(fd: &AsyncFd<UnixStream>) -> usize {
        let chunk = [5u8; 4096];
        let mut sent = 0;
        loop {
            match fd.get_ref().write(&chunk) {
                Ok(n) => sent += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return sent,
                Err(err) => panic!("{err}"),
            }
        }
    }

    /// As above, on the write side: the peer drains the socket between a
    /// write that would block and the clear.
    #[test]
    fn a_write_edge_after_the_call_survives_the_clear() {
        crate::block_on(async {
            let (a, mut b) = UnixStream::pair().unwrap();
            a.set_nonblocking(true).unwrap();
            let fd = AsyncFd::with_interest(a, Interest::WRITABLE).unwrap();
            let mut sink = vec![0u8; 1 << 20];
            // A wait that waits registers the descriptor with the driver.
            let mut guard = fd.writable().await.unwrap();
            let sent = fill(&fd);
            guard.clear_ready();
            b.read_exact(&mut sink[..sent]).unwrap();
            fd.writable().await.unwrap();
            let mut first = fd.writable().await.unwrap();
            let sent = fill(&fd);
            b.read_exact(&mut sink[..sent]).unwrap();
            if on_ring() {
                first.clear_ready();
                let n = fd.async_io(Interest::WRITABLE, |mut a| a.write(b"w"));
                assert_eq!(n.await.unwrap(), 1);
                return;
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                crate::task::yield_now().await;
                let look = fd.writable().await.unwrap();
                if look.marks.write != first.marks.write {
                    break;
                }
                assert!(Instant::now() < deadline, "the driver never saw the edge");
            }
            first.clear_ready();
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(
                fd.poll_write_ready(&mut cx).is_ready(),
                "the edge after the call was lost to the clear"
            );
        });
    }
}
