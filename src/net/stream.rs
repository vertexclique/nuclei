//! The byte streams' calls, on the handle and on a shared reference to it:
//! `AsyncRead` and `AsyncWrite`, the reads and writes that lend the driver
//! a buffer of the caller's (and for TCP, the zero-copy write), readiness,
//! the reads and writes that do not wait, and a stream's halves. A read
//! side, a write side and a stream (both, with its halves) each have their
//! macro: a socket stream takes all three, a pipe's end one side.
//!
//! Ready means the handle's own buffers: readable once a read would end at
//! once (bytes read ahead, the end of the stream, or an error, which the
//! read reports), writable once a write would be taken at once (room in the
//! write-behind, or an error).

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use futures_io::{AsyncRead, AsyncWrite};

use super::Released;
use super::split::{OwnedReadHalf, OwnedWriteHalf, ReadHalf, WriteHalf};
use crate::io::{Handle, Interest, Ready};

/// The read side of a byte stream handle.
macro_rules! read_side {
    ($($ty:ty),* $(,)?) => {$(
        impl Handle<$ty> {
            /// Reads into the spare capacity of `buf`, after the bytes it
            /// holds, and gives `buf` back with how many bytes came: zero at
            /// the end of the stream, or when `buf` has no spare capacity.
            /// The driver reads into `buf` itself, so the bytes are never
            /// copied.
            ///
            /// If the future is dropped before it completes, bytes the
            /// driver read into `buf` stay with the handle for its next
            /// read, so none is lost.
            pub async fn read_owned(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
                self.source().read_owned(buf).await
            }

            /// Waits until a read would end at once.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub async fn readable(&self) -> io::Result<()> {
                core::future::poll_fn(|cx| self.poll_read_ready(cx)).await
            }

            /// Polls until a read would end at once.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.source().poll_read_ready(cx).map(|r| r.map(drop))
            }

            /// Reads what came ahead into `buf` without waiting; zero at the
            /// end of the stream.
            ///
            /// # Errors
            ///
            /// `WouldBlock` when nothing came yet (the driver goes on reading,
            /// so a later call finds what comes), and as `read(2)`.
            pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
                super::at_once(|cx| self.source().poll_read(cx, buf, false))
            }

            /// Reads what came ahead into `bufs` without waiting, as
            /// [`try_read`](Self::try_read).
            ///
            /// # Errors
            ///
            /// As [`try_read`](Self::try_read).
            pub fn try_read_vectored(&self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
                super::at_once(|cx| self.source().poll_read_vectored(cx, bufs))
            }
        }

        impl AsyncRead for &Handle<$ty> {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_read(cx, buf, false)
            }

            fn poll_read_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &mut [io::IoSliceMut<'_>],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_read_vectored(cx, bufs)
            }
        }

        impl AsyncRead for Handle<$ty> {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_read(cx, buf, false)
            }

            fn poll_read_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &mut [io::IoSliceMut<'_>],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_read_vectored(cx, bufs)
            }
        }
    )*};
}

/// The write side of a byte stream handle.
macro_rules! write_side {
    ($($ty:ty),* $(,)?) => {$(
        impl Handle<$ty> {
            /// Writes all of `buf` and gives it back with its length. The
            /// driver writes from `buf` itself, so the bytes are never
            /// copied, and the write ends once the kernel took the last of
            /// them.
            ///
            /// If the future is dropped before it completes, the stream
            /// gets `buf` whole or not at all: a write none of which went
            /// out is withdrawn, and one that started is finished by the
            /// driver. On Windows a TCP write is always finished: there a
            /// cancelled send may have handed bytes to the stack while it
            /// says it took none.
            pub async fn write_owned(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
                self.source().write_owned(buf).await
            }

            /// Waits until a write would be taken at once.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub async fn writable(&self) -> io::Result<()> {
                core::future::poll_fn(|cx| self.poll_write_ready(cx)).await
            }

            /// Polls until a write would be taken at once.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.source().poll_write_ready(cx).map(|r| r.map(drop))
            }

            /// Hands `buf` to the write-behind without waiting; returns how
            /// many bytes it took.
            ///
            /// # Errors
            ///
            /// `WouldBlock` when the write-behind has no room, and the error
            /// that stopped earlier bytes.
            pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
                super::at_once(|cx| self.source().poll_write(cx, buf))
            }

            /// Hands the bytes of `bufs`, in order, to the write-behind
            /// without waiting, as [`try_write`](Self::try_write).
            ///
            /// # Errors
            ///
            /// As [`try_write`](Self::try_write).
            pub fn try_write_vectored(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
                super::at_once(|cx| self.source().poll_write_vectored(cx, bufs))
            }
        }

        impl AsyncWrite for &Handle<$ty> {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_write(cx, buf)
            }

            fn poll_write_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &[io::IoSlice<'_>],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_write_vectored(cx, bufs)
            }

            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.source().poll_flush(cx)
            }

            fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.source().poll_close(cx)
            }
        }

        impl AsyncWrite for Handle<$ty> {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_write(cx, buf)
            }

            fn poll_write_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &[io::IoSlice<'_>],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_write_vectored(cx, bufs)
            }

            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.source().poll_flush(cx)
            }

            fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.source().poll_close(cx)
            }
        }
    )*};
}

/// A stream that both reads and writes: its readiness of either, and its
/// halves.
macro_rules! stream_io {
    ($($ty:ty),* $(,)?) => {$(
        read_side!($ty);
        write_side!($ty);

        impl Handle<$ty> {
            /// The stream's two halves, borrowed: to read on one task and
            /// write on another. Nothing is allocated.
            pub fn split(&self) -> (ReadHalf<'_, $ty>, WriteHalf<'_, $ty>) {
                (ReadHalf(self), WriteHalf(self))
            }

            /// The stream's two halves, owning it together; see
            /// [`OwnedReadHalf::reunite`].
            pub fn into_split(self) -> (OwnedReadHalf<$ty>, OwnedWriteHalf<$ty>) {
                let both = Arc::new(self);
                (OwnedReadHalf(Arc::clone(&both)), OwnedWriteHalf(both))
            }

            /// Waits until some of `interest` is ready, and says what is.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
                core::future::poll_fn(|cx| super::poll_ready(self.source(), cx, interest)).await
            }
        }
    )*};
}

stream_io!(TcpStream);

impl Handle<TcpStream> {
    /// Writes all of `buf` as [`write_owned`](Self::write_owned) does, with the kernel's
    /// zero-copy send where it has one, and returns the write's result with a [`Released`] that
    /// gives `buf` back once the kernel let go of it.
    ///
    /// The write ends once the kernel took the last byte, as `write_owned`'s does, but the kernel
    /// keeps reading `buf` until the peer acknowledged every byte; only then does the `Released`
    /// resolve to it. So a peer that writes before it reads cannot hold the write up, and writes
    /// pipeline: the next one may start while earlier buffers are still out.
    ///
    /// A write of less than the runtime's
    /// [`zero_copy_threshold`](crate::runtime::Builder::zero_copy_threshold) (1 MiB by default),
    /// where copying costs less, and every write on epoll, kqueue,
    /// I/O completion ports or an io_uring kernel without zero-copy sends (or without memory it
    /// may lock for them), is copied by the kernel as `write_owned`'s is, and its `Released` is
    /// ready at once. So is a write served by a ring only its own thread may reap: a thread's own
    /// shard (a stream polled by `block_on` outside the runtime's workers), or the workers of a
    /// runtime built with [`handoff_after(None)`](crate::runtime::Builder::handoff_after). Their
    /// rings could not be handed to another thread, which a buffer still out when the thread
    /// stops would need.
    ///
    /// If the future is dropped before it completes, the stream gets `buf` whole or not at all,
    /// as with `write_owned`. Either way `buf` is dropped only once the kernel let go of it.
    pub async fn write_zero_copy(&self, buf: Vec<u8>) -> (io::Result<usize>, Released) {
        self.source().write_zero_copy(buf).await
    }
}
#[cfg(unix)]
stream_io!(UnixStream);
#[cfg(unix)]
read_side!(std::io::PipeReader);
#[cfg(unix)]
write_side!(std::io::PipeWriter);
