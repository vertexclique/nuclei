//! `AsyncRead` and `AsyncWrite` for the stream sockets, on the handle and on
//! a shared reference to it, and the reads and writes that lend the driver
//! a buffer of the caller's.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

use futures_io::{AsyncRead, AsyncWrite};

use std::sync::Arc;

use super::split::{OwnedReadHalf, OwnedWriteHalf, ReadHalf, WriteHalf};
use crate::io::{Handle, Interest, Ready};

macro_rules! stream_io {
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
        }

        /// Readiness, and the reads and writes that do not wait. Ready means
        /// the handle's own buffers: readable once a read would end at once
        /// (bytes read ahead, the end of the stream, or an error, which the
        /// read reports), writable once a write would be taken at once (room
        /// in the write-behind, or an error).
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

            /// Waits until a read would end at once.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub async fn readable(&self) -> io::Result<()> {
                self.ready(Interest::READABLE).await.map(drop)
            }

            /// Waits until a write would be taken at once.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub async fn writable(&self) -> io::Result<()> {
                self.ready(Interest::WRITABLE).await.map(drop)
            }

            /// Waits until some of `interest` is ready, and says what is.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
                core::future::poll_fn(|cx| super::poll_ready(self.source(), cx, interest)).await
            }

            /// Polls until a read would end at once.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.source().poll_read_ready(cx).map(|r| r.map(drop))
            }

            /// Polls until a write would be taken at once.
            ///
            /// # Errors
            ///
            /// When the stream cannot be watched.
            pub fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.source().poll_write_ready(cx).map(|r| r.map(drop))
            }

            /// Reads what came ahead into `buf` without waiting; zero at the
            /// end of the stream.
            ///
            /// # Errors
            ///
            /// `WouldBlock` when nothing came yet (the driver goes on reading,
            /// so a later call finds what comes), and as `recv(2)`.
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

stream_io!(TcpStream);
#[cfg(unix)]
stream_io!(UnixStream);
