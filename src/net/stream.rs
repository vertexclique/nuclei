//! `AsyncRead` and `AsyncWrite` for the stream sockets, on the handle and on
//! a shared reference to it, and the reads and writes that lend the driver
//! a buffer of the caller's.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::net::TcpStream;
use std::os::unix::net::UnixStream;

use futures_io::{AsyncRead, AsyncWrite};

use crate::io::Handle;

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
            /// driver.
            pub async fn write_owned(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
                self.source().write_owned(buf).await
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

stream_io!(TcpStream, UnixStream);
