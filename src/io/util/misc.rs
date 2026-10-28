//! Readers and writers with nothing behind them: [`empty`], [`repeat`] and
//! [`sink`].

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

use futures_io::{AsyncBufRead, AsyncRead, AsyncWrite};

/// A reader at its end at once, and a writer that takes every byte and
/// keeps none. Made by [`empty`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Empty {
    _private: (),
}

/// A reader at its end at once, and a writer that takes every byte and
/// keeps none.
pub fn empty() -> Empty {
    Empty { _private: () }
}

impl AsyncRead for Empty {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        _: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(0))
    }
}

impl AsyncBufRead for Empty {
    fn poll_fill_buf(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        Poll::Ready(Ok(&[]))
    }

    fn consume(self: Pin<&mut Self>, _: usize) {}
}

impl AsyncWrite for Empty {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A reader that gives one byte over and over, without end. Made by
/// [`repeat`].
#[derive(Clone, Copy, Debug)]
pub struct Repeat {
    byte: u8,
}

/// A reader that gives `byte` over and over, without end.
pub fn repeat(byte: u8) -> Repeat {
    Repeat { byte }
}

impl AsyncRead for Repeat {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // Ready every time: it spends the task's budget, as IO does, so a
        // loop over it yields to the other tasks.
        crate::coop::poll_budget(cx, |_| {
            buf.fill(self.byte);
            Poll::Ready(Ok(buf.len()))
        })
    }
}

/// A writer that takes every byte and keeps none. Made by [`sink`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Sink {
    _private: (),
}

/// A writer that takes every byte and keeps none.
pub fn sink() -> Sink {
    Sink { _private: () }
}

impl AsyncWrite for Sink {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        crate::coop::poll_budget(cx, |_| Poll::Ready(Ok(buf.len())))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
