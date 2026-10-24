//! `AsyncRead` and `AsyncWrite` for the stream sockets, on the handle and on
//! a shared reference to it.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::net::TcpStream;
use std::os::unix::net::UnixStream;

use futures_io::{AsyncRead, AsyncWrite};

use crate::io::Handle;

macro_rules! stream_io {
    ($($ty:ty),* $(,)?) => {$(
        impl AsyncRead for &Handle<$ty> {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_read(cx, buf, false)
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
        }

        impl AsyncWrite for Handle<$ty> {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                self.source().poll_write(cx, buf)
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
