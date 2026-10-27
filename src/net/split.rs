//! A stream's two halves, to read on one task and write on another:
//! borrowed ([`ReadHalf`], [`WriteHalf`], from `split`, allocating nothing)
//! or owned ([`OwnedReadHalf`], [`OwnedWriteHalf`], from `into_split`, which
//! share the stream in one `Arc`), and owned halves made one stream again
//! with [`OwnedReadHalf::reunite`].
//!
//! A stream is read and written through a shared reference already, so the
//! halves only keep each side to its task. Dropping the write half does not
//! end the write side, as bytes may still wait in the write-behind:
//! `close` (`AsyncWriteExt::close`) sends them, then ends it.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::fmt;
use std::io;
use std::sync::Arc;

use futures_io::{AsyncRead, AsyncWrite};

use crate::io::Handle;

/// The read half of a stream, borrowed from it.
pub struct ReadHalf<'a, S>(pub(super) &'a Handle<S>);

/// The write half of a stream, borrowed from it.
pub struct WriteHalf<'a, S>(pub(super) &'a Handle<S>);

/// The read half of a stream, owning it with the write half.
pub struct OwnedReadHalf<S>(pub(super) Arc<Handle<S>>);

/// The write half of a stream, owning it with the read half.
pub struct OwnedWriteHalf<S>(pub(super) Arc<Handle<S>>);

/// Two halves of different streams, given back by
/// [`OwnedReadHalf::reunite`].
pub struct ReuniteError<S>(pub OwnedReadHalf<S>, pub OwnedWriteHalf<S>);

impl<S> OwnedReadHalf<S> {
    /// The stream again, from its two halves.
    ///
    /// # Errors
    ///
    /// When `other` is a half of another stream; both are given back.
    pub fn reunite(self, other: OwnedWriteHalf<S>) -> Result<Handle<S>, ReuniteError<S>> {
        if !Arc::ptr_eq(&self.0, &other.0) {
            return Err(ReuniteError(self, other));
        }
        drop(other);
        // The two were the only holders (halves are not cloned): the stream
        // is this one's alone now.
        Arc::try_unwrap(self.0)
            .map_err(|both| ReuniteError(OwnedReadHalf(Arc::clone(&both)), OwnedWriteHalf(both)))
    }
}

impl<S> OwnedWriteHalf<S> {
    /// The stream again, from its two halves.
    ///
    /// # Errors
    ///
    /// When `other` is a half of another stream; both are given back.
    pub fn reunite(self, other: OwnedReadHalf<S>) -> Result<Handle<S>, ReuniteError<S>> {
        other.reunite(self)
    }
}

impl<S> AsRef<Handle<S>> for ReadHalf<'_, S> {
    fn as_ref(&self) -> &Handle<S> {
        self.0
    }
}

impl<S> AsRef<Handle<S>> for WriteHalf<'_, S> {
    fn as_ref(&self) -> &Handle<S> {
        self.0
    }
}

impl<S> AsRef<Handle<S>> for OwnedReadHalf<S> {
    fn as_ref(&self) -> &Handle<S> {
        &self.0
    }
}

impl<S> AsRef<Handle<S>> for OwnedWriteHalf<S> {
    fn as_ref(&self) -> &Handle<S> {
        &self.0
    }
}

macro_rules! debug_half {
    ($($half:ident $(<$lt:lifetime>)?),*) => {$(
        impl<$($lt,)? S: fmt::Debug> fmt::Debug for $half<$($lt,)? S> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($half)).field(self.as_ref()).finish()
            }
        }
    )*};
}

debug_half!(ReadHalf<'a>, WriteHalf<'a>, OwnedReadHalf, OwnedWriteHalf);

impl<S> fmt::Debug for ReuniteError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReuniteError(..)")
    }
}

impl<S> fmt::Display for ReuniteError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the two halves are of different streams")
    }
}

impl<S> std::error::Error for ReuniteError<S> {}

macro_rules! read_half {
    ($($half:ident $(<$lt:lifetime>)?),*) => {$(
        impl<$($lt,)? S> AsyncRead for $half<$($lt,)? S>
        where
            for<'x> &'x Handle<S>: AsyncRead,
        {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut self.get_mut().as_ref()).poll_read(cx, buf)
            }

            fn poll_read_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &mut [io::IoSliceMut<'_>],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut self.get_mut().as_ref()).poll_read_vectored(cx, bufs)
            }
        }
    )*};
}

macro_rules! write_half {
    ($($half:ident $(<$lt:lifetime>)?),*) => {$(
        impl<$($lt,)? S> AsyncWrite for $half<$($lt,)? S>
        where
            for<'x> &'x Handle<S>: AsyncWrite,
        {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut self.get_mut().as_ref()).poll_write(cx, buf)
            }

            fn poll_write_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &[io::IoSlice<'_>],
            ) -> Poll<io::Result<usize>> {
                Pin::new(&mut self.get_mut().as_ref()).poll_write_vectored(cx, bufs)
            }

            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Pin::new(&mut self.get_mut().as_ref()).poll_flush(cx)
            }

            fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                Pin::new(&mut self.get_mut().as_ref()).poll_close(cx)
            }
        }
    )*};
}

read_half!(ReadHalf<'a>, OwnedReadHalf);
write_half!(WriteHalf<'a>, OwnedWriteHalf);
