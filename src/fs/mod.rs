//! Files, as [`Handle<File>`](crate::io::Handle).
//!
//! A regular file's reads and writes run on the blocking pool at the
//! handle's own cursor (see `file`); a `File` that is not a regular file (a
//! FIFO, a terminal) is watched by the shard's poller like a socket. Reads
//! and writes are `AsyncRead` and `AsyncWrite` on the handle and on a shared
//! reference to it (both share the cursor, as `&File` shares its offset);
//! `AsyncBufRead` and `AsyncSeek` are on the handle.

pub(crate) mod file;

use core::pin::Pin;
use core::task::{Context, Poll};
use std::fs::File;
use std::io::{self, SeekFrom};
use std::path::Path;

use futures_io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite};

use crate::io::Handle;
use crate::io::source::Kind;

impl Handle<File> {
    /// Opens the file at `p` for reading, on the blocking pool.
    ///
    /// # Errors
    ///
    /// As [`File::open`].
    pub async fn open(p: impl AsRef<Path>) -> io::Result<Handle<File>> {
        let path = p.as_ref().to_path_buf();
        let file = crate::runtime::spawn_blocking(move || File::open(path)).await?;
        Handle::new(file)
    }

    fn is_regular(&self) -> bool {
        self.source.kind() == Kind::File
    }
}

macro_rules! file_io {
    ($($ty:ty),*) => {$(
        impl AsyncRead for $ty {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut [u8],
            ) -> Poll<io::Result<usize>> {
                if !self.is_regular() {
                    return self.source().poll_read(cx, buf, false);
                }
                match self.file() {
                    Ok(file) => file.poll_read(cx, buf),
                    Err(err) => Poll::Ready(Err(err)),
                }
            }
        }

        impl AsyncWrite for $ty {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                if !self.is_regular() {
                    return self.source().poll_write(cx, buf);
                }
                match self.file() {
                    Ok(file) => file.poll_write(cx, buf),
                    Err(err) => Poll::Ready(Err(err)),
                }
            }

            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                if !self.is_regular() {
                    return self.source().poll_flush(cx);
                }
                match self.file() {
                    Ok(file) => file.poll_flush(cx),
                    Err(err) => Poll::Ready(Err(err)),
                }
            }

            fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                // A file has no half to shut down: closing is flushing.
                self.poll_flush(cx)
            }
        }
    )*};
}

file_io!(Handle<File>, &Handle<File>);

impl AsyncSeek for Handle<File> {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        if !self.is_regular() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the file is a stream, which has no position to seek",
            )));
        }
        match self.file() {
            Ok(file) => file.poll_seek(cx, pos),
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

impl AsyncBufRead for Handle<File> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = Pin::into_inner(self);
        if !this.is_regular() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "buffered reads are for regular files; wrap a stream in a buffered reader",
            )));
        }
        let file = match this.file() {
            Ok(file) => file,
            Err(err) => return Poll::Ready(Err(err)),
        };
        file.poll_fill_buf(cx).map_ok(|(ptr, len)| {
            // SAFETY: the bytes are read ahead into a pool buffer the state
            // keeps until a call that consumes, writes or seeks; each of
            // those needs this handle, which the slice borrows exclusively,
            // and no job writes into a buffer the state holds.
            unsafe { core::slice::from_raw_parts(ptr, len) }
        })
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        if let Some(file) = self.file.get() {
            file.consume(amt);
        }
    }
}

#[cfg(test)]
mod tests;
