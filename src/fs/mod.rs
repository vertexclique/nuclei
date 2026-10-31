//! The file system: files as [`File`] (std's file in a
//! [`Handle`]), [`OpenOptions`], the calls by path
//! ([`read`], [`write`](fn@write), [`metadata`], [`rename`], [`copy`], ...),
//! directories ([`create_dir_all`], [`DirBuilder`], [`read_dir`] streamed a
//! page at a time) and links. Calls by path, and a regular file's calls
//! beside its reads, writes and syncs, run on the blocking pool.
//!
//! A regular file's reads and writes, at the handle's own cursor or at an
//! offset of the caller's ([`Handle::read_at`], [`Handle::write_at`] and
//! their forms, which many tasks may make at once on one handle), and its
//! syncs ([`Handle::sync_all`], [`Handle::sync_data`]), are operations of
//! the shard's ring on io_uring and jobs of the blocking pool on the other
//! backends (see `file`). A file opened for direct IO (`O_DIRECT`,
//! `FILE_FLAG_NO_BUFFERING`) is read and written with exactly the caller's
//! lengths and offsets, never through the handle's read-ahead or
//! write-behind. A `File` that is not a regular file (a FIFO, a terminal, a
//! pipe) is a byte stream the shard's driver serves like a socket (on
//! Windows, through the completion port, or through jobs of the blocking
//! pool for a handle opened for synchronous IO). Reads and writes at the
//! cursor are `AsyncRead` and `AsyncWrite` on the handle and on a shared
//! reference to it (both share the cursor, as `&File` shares its offset);
//! `AsyncBufRead` and `AsyncSeek` are on the handle. `read_owned`,
//! `write_owned`, `read_at_owned` and `write_at_owned` take the caller's
//! buffer and give it back, so the bytes are read into it and written from
//! it with no copy.
//!
//! Beside those, a regular file can be asked to change the space a range
//! uses ([`Handle::allocate`]: reserve, keep the length, punch a hole, zero),
//! to write a range back ([`Handle::sync_range`]), to copy a range into
//! another file in the kernel where the system can ([`Handle::copy_range_to`]),
//! and to take an advisory lock without blocking the worker
//! ([`Handle::lock`], [`Handle::try_lock`], [`Handle::unlock`]).
//!
//! A file's name in its directory needs [`sync_dir`] to survive a crash. To
//! replace a file atomically: write a temporary file in the same directory,
//! `sync_all` it, [`rename`] it over the target, then [`sync_dir`] the
//! directory.

mod allocate;
pub(crate) mod call;
mod copy_range;
pub(crate) mod file;
mod file_ops;
mod lock;
mod ops;
mod read_dir;
#[cfg(ring)]
pub(crate) mod ring;
mod sync_dir;

pub use self::allocate::Allocate;
pub use std::fs::{FileTimes, FileType, Metadata, Permissions, TryLockError};

#[cfg(unix)]
pub use self::ops::symlink;
pub use self::ops::{
    DirBuilder, OpenOptions, canonicalize, copy, create_dir, create_dir_all, hard_link, metadata,
    read, read_link, read_to_string, remove_dir, remove_dir_all, remove_file, rename,
    set_permissions, symlink_metadata, try_exists, write,
};
#[cfg(windows)]
pub use self::ops::{symlink_dir, symlink_file};
pub use self::read_dir::{DirEntry, ReadDir, read_dir};
pub use self::sync_dir::sync_dir;

/// A file: std's `File` in a [`Handle`], with async reads, writes, seeks and
/// calls.
pub type File = Handle<std::fs::File>;

use core::pin::Pin;
use core::task::{Context, Poll};
use std::fs::File as StdFile;
use std::io::{self, SeekFrom};
use std::path::Path;

use futures_io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite};

use crate::buf::owned::{IoBuf, IoBufMut, OwnedBuf};
use crate::buf::vectored::{self, IoVecBuf, IoVecBufMut};
use crate::io::Handle;
use crate::io::source::Kind;

/// A file's calls. std's own `File::lock`, `FileExt::read_at` and
/// `FileExt::write_at` (on Windows `seek_read` and `seek_write`), reached
/// through `Deref`, block the worker thread that makes them. Use the async
/// calls instead: [`lock`](Handle::lock), [`try_lock`](Handle::try_lock) and
/// [`unlock`](Handle::unlock), [`read_at`](Handle::read_at) and
/// [`write_at`](Handle::write_at), and, with no copy of the bytes,
/// [`read_at_owned`](Handle::read_at_owned) and
/// [`write_at_owned`](Handle::write_at_owned).
impl Handle<StdFile> {
    /// Opens the file at `p` for reading, on the blocking pool.
    ///
    /// # Errors
    ///
    /// As [`File::open`].
    pub async fn open(p: impl AsRef<Path>) -> io::Result<Handle<StdFile>> {
        let path = p.as_ref().to_path_buf();
        let file = ops::blocking(move || StdFile::open(path)).await?;
        Handle::new(file)
    }

    fn is_regular(&self) -> bool {
        self.source.kind() == Kind::File
    }

    /// Reads at the cursor into the spare capacity of `buf`, after the bytes
    /// it holds, and gives `buf` back with how many bytes came: zero at the
    /// end of the file, or when `buf` has no spare capacity. The bytes are
    /// read into `buf` itself, never copied, once the writes before the read
    /// landed.
    ///
    /// If the future is dropped before it completes, the cursor stays where
    /// it was. `buf` is any [`IoBufMut`]: a `Vec<u8>`, an
    /// [`AlignedBuf`](crate::io::AlignedBuf) for a file opened for direct IO,
    /// or a type of the caller's.
    pub async fn read_owned<B>(&self, buf: B) -> (io::Result<usize>, B)
    where
        B: IoBufMut,
    {
        if !self.is_regular() {
            return self.source().read_owned(buf).await;
        }
        match self.file() {
            Ok(file) => file.read_owned(buf).await,
            Err(err) => (Err(err), buf),
        }
    }

    /// Writes all of `buf` at the cursor (at the end, for a file opened to
    /// append), after the writes before it, and gives it back with its
    /// length once the bytes reached the file. They are written from `buf`
    /// itself, never copied.
    ///
    /// If the future is dropped before it completes, the write is withdrawn
    /// while it still waits for the writes before it, and lands whole, in
    /// its place, once it started. `buf` is any [`IoBuf`], as for
    /// [`read_owned`](Self::read_owned).
    pub async fn write_owned<B>(&self, buf: B) -> (io::Result<usize>, B)
    where
        B: IoBuf,
    {
        if !self.is_regular() {
            return self.source().write_owned(buf).await;
        }
        match self.file() {
            Ok(file) => file.write_owned(buf).await,
            Err(err) => (Err(err), buf),
        }
    }

    /// Reads at the cursor into the spare capacity of the first of `bufs`
    /// that has any, as [`read_owned`](Self::read_owned) does, and gives
    /// `bufs` back with how many bytes came: zero at the end of the file, or
    /// when no buffer has spare capacity. It is a short read, as std's own
    /// `read_vectored` is for a reader that reads one buffer at a time.
    pub async fn read_vectored_owned<V>(&self, bufs: V) -> (io::Result<usize>, V)
    where
        V: IoVecBufMut,
    {
        let mut bufs = vectored::filled(bufs);
        let Some(room) = bufs.iter_mut().find(|b| b.len() < b.capacity()) else {
            return (Ok(0), vectored::into_filled(bufs));
        };
        let (result, buf) = self.read_owned(core::mem::take(room)).await;
        *room = buf;
        (result, vectored::into_filled(bufs))
    }

    /// Writes all of the bytes of `bufs`, each buffer's in order, at the
    /// cursor as one write (as [`write_owned`](Self::write_owned) writes
    /// one buffer), and gives `bufs` back with how many bytes that was. No
    /// other write of the handle comes between two of them. They are written
    /// from the buffers themselves, never copied, each buffer in calls of its
    /// own; on a file opened for direct IO each buffer's memory, length and
    /// place in the file meet the kernel's alignment rules on their own.
    pub async fn write_vectored_owned<V>(&self, bufs: V) -> (io::Result<usize>, V)
    where
        V: IoVecBuf,
    {
        let bufs = OwnedBuf::vectored(vectored::written(bufs));
        let (result, bufs) = self.write_owned(bufs).await;
        (result, vectored::into_written(bufs.into_segments()))
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

            fn poll_read_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &mut [io::IoSliceMut<'_>],
            ) -> Poll<io::Result<usize>> {
                if !self.is_regular() {
                    return self.source().poll_read_vectored(cx, bufs);
                }
                match self.file() {
                    Ok(file) => file.poll_read_vectored(cx, bufs),
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

            fn poll_write_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &[io::IoSlice<'_>],
            ) -> Poll<io::Result<usize>> {
                if !self.is_regular() {
                    return self.source().poll_write_vectored(cx, bufs);
                }
                match self.file() {
                    Ok(file) => file.poll_write_vectored(cx, bufs),
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

file_io!(Handle<StdFile>, &Handle<StdFile>);

impl AsyncSeek for Handle<StdFile> {
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

impl AsyncBufRead for Handle<StdFile> {
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
