//! A file's calls beside its reads and writes: creating it, syncing it,
//! its length, metadata, permissions and times, and a second handle to it.
//!
//! Each runs on the blocking pool after the handle's writes in flight landed
//! (so a length sees them), on the handle's own descriptor, which a lease
//! keeps open for the call: one dropped meanwhile leaves its object to the
//! call, and none needs a descriptor of its own. A regular file's sync goes
//! to the file's own IO state instead (`fs::file::durable`), on the shard's
//! ring where it runs one.

use core::pin::Pin;
use std::fs::{File, FileTimes, Metadata, Permissions};
use std::io;
use std::path::Path;
use std::time::SystemTime;

use futures_io::AsyncWrite;

use super::Allocate;
use super::file::{SyncKind, sync_file};
use super::ops::{OpenOptions, blocking};
use crate::io::Handle;
use crate::io::raw::Raw;
use crate::io::sys;

/// The descriptor of `file`, as the system calls take it.
#[cfg(unix)]
fn raw_of(file: &File) -> Raw {
    std::os::fd::AsRawFd::as_raw_fd(file)
}

/// The handle of `file`, as the system calls take it.
#[cfg(windows)]
fn raw_of(file: &File) -> Raw {
    std::os::windows::io::AsRawHandle::as_raw_handle(file) as usize as Raw
}

impl Handle<File> {
    /// Opens the file at `path` for writing, creating it or cutting it down
    /// to nothing.
    pub async fn create(path: impl AsRef<Path>) -> io::Result<Handle<File>> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await
    }

    /// Creates the file at `path` and opens it for reading and writing,
    /// failing if it exists.
    pub async fn create_new(path: impl AsRef<Path>) -> io::Result<Handle<File>> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .await
    }

    /// The options to open a file with.
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    /// Writes the file's data and metadata through to the device, as std's
    /// `File::sync_all` does, once every write accepted before the call (at
    /// the cursor, or positional and returned) landed. On io_uring the sync
    /// is an operation of the shard's ring (`IORING_OP_FSYNC`); elsewhere a
    /// job of the blocking pool.
    ///
    /// # Errors
    ///
    /// A write that failed (reported here as by a flush), or the sync's own
    /// failure. Once a sync of the handle failed, every later one fails with
    /// that error: the system reports a failure to write data back once, so
    /// a later sync that succeeded would not mean the data reached the
    /// device. Open the file again to start over.
    pub async fn sync_all(&self) -> io::Result<()> {
        if !self.is_regular() {
            return self.on_pool(File::sync_all).await;
        }
        self.file()?.sync(SyncKind::All).await
    }

    /// Writes the file's data through to the device, and the metadata a
    /// read needs, as std's `File::sync_data` does (`IORING_FSYNC_DATASYNC`
    /// on io_uring); otherwise as [`sync_all`](Self::sync_all).
    ///
    /// # Errors
    ///
    /// As [`sync_all`](Self::sync_all).
    pub async fn sync_data(&self) -> io::Result<()> {
        if !self.is_regular() {
            return self.on_pool(File::sync_data).await;
        }
        self.file()?.sync(SyncKind::Data).await
    }

    /// Writes back the dirty data of the `len` bytes of the file from
    /// `offset` and waits for it (`len` of zero: to the end of the file), as
    /// Linux's `sync_file_range` does, once every write accepted before the
    /// call landed. On io_uring this is an operation of the shard's ring
    /// (`IORING_OP_SYNC_FILE_RANGE`, for a range its length field holds, else
    /// a job of the blocking pool).
    ///
    /// This is not a durability guarantee: it writes no metadata and flushes
    /// no cache of the device, so a crash may still lose the data. Use
    /// [`sync_data`](Self::sync_data) for that. On the systems that have no
    /// such call (macOS, Windows, other unix) it does what `sync_data` does
    /// for the whole file, which is stronger and covers the range.
    ///
    /// # Errors
    ///
    /// As [`sync_all`](Self::sync_all): a write that failed, or the call's
    /// own failure, which every later sync of the handle then repeats (the
    /// system reports a write-back error to one call only).
    pub async fn sync_range(&self, offset: u64, len: u64) -> io::Result<()> {
        let kind = SyncKind::Range { offset, len };
        if !self.is_regular() {
            return self.on_pool(move |file| sync_file(file, kind)).await;
        }
        self.file()?.sync(kind).await
    }

    /// Changes the space the `len` bytes of the file from `offset` use, as
    /// `mode` says: reserves it, keeps the length, frees it, or zeroes it.
    /// The call is a write of the range: it lands after every write accepted
    /// at the cursor before it and every positional write that returned
    /// before it, a read at the cursor made after it sees what it did, and a
    /// future dropped while the call is out leaves it to the file, which
    /// every later call of the handle waits for. On io_uring it is an
    /// operation of the shard's ring (`fallocate(2)`); elsewhere a job of the
    /// blocking pool (`fcntl` on macOS, `DeviceIoControl` and
    /// `SetFileInformationByHandle` on Windows).
    ///
    /// A system that cannot do `mode` answers `Unsupported` (macOS cannot
    /// zero a range in place), and a file system may refuse a mode as the
    /// system says.
    ///
    /// # Errors
    ///
    /// `InvalidInput` when the end of the range is past the largest offset a
    /// file can have; otherwise the system's own answer, among them that of
    /// an empty range, a handle that cannot be written, and a stream.
    pub async fn allocate(&self, offset: u64, len: u64, mode: Allocate) -> io::Result<()> {
        if !self.is_regular() {
            return self
                .on_pool(move |file| sys::allocate(raw_of(file), offset, len, mode))
                .await;
        }
        self.file()?.allocate(offset, len, mode).await
    }

    /// Sets the file's length: cut down, or grown with zeros. The cursor
    /// stays where it is. Bytes of the file read ahead before the cut are
    /// never handed out after it.
    pub async fn set_len(&self, len: u64) -> io::Result<()> {
        let result = self.on_pool(move |file| file.set_len(len)).await;
        if let Some(file) = self.file.get() {
            file.moved_on();
        }
        result
    }

    /// The file's metadata.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        self.on_pool(File::metadata).await
    }

    /// Sets the file's permissions.
    pub async fn set_permissions(&self, perm: Permissions) -> io::Result<()> {
        self.on_pool(move |file| file.set_permissions(perm)).await
    }

    /// Sets the file's access and modification times.
    pub async fn set_times(&self, times: FileTimes) -> io::Result<()> {
        self.on_pool(move |file| file.set_times(times)).await
    }

    /// Sets the file's modification time.
    pub async fn set_modified(&self, time: SystemTime) -> io::Result<()> {
        self.on_pool(move |file| file.set_modified(time)).await
    }

    /// A second handle to the same open file, with a cursor of its own
    /// starting where the file's offset is (writes in flight landed first).
    pub async fn try_clone(&self) -> io::Result<Handle<File>> {
        let file = self.on_pool(File::try_clone).await?;
        Handle::new(file)
    }

    /// Runs `call` on the blocking pool once the writes in flight landed, on
    /// the handle's own descriptor, leased for the call.
    async fn on_pool<T, F>(&self, call: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&File) -> io::Result<T> + Send + 'static,
    {
        let mut this = self;
        core::future::poll_fn(|cx| Pin::new(&mut this).poll_flush(cx)).await?;
        let lease = self.source().lease()?;
        blocking(move || call(&lease.file())).await
    }
}
