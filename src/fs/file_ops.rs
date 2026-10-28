//! A file's calls beside its reads and writes: creating it, syncing it,
//! its length, metadata, permissions and times, and a second handle to it.
//!
//! Each runs on the blocking pool after the handle's writes in flight landed
//! (so a sync or a length sees them), on a duplicate of the file's
//! descriptor, so the call never runs on a descriptor the handle closed.

use core::pin::Pin;
use std::fs::{File, FileTimes, Metadata, Permissions};
use std::io;
use std::path::Path;
use std::time::SystemTime;

use futures_io::AsyncWrite;

use super::ops::{OpenOptions, blocking};
use crate::io::Handle;

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

    /// Writes the file's data and metadata through to the device.
    pub async fn sync_all(&self) -> io::Result<()> {
        self.on_pool(File::sync_all).await
    }

    /// Writes the file's data through to the device, and the metadata a
    /// read needs.
    pub async fn sync_data(&self) -> io::Result<()> {
        self.on_pool(File::sync_data).await
    }

    /// Sets the file's length: cut down, or grown with zeros. The cursor
    /// stays where it is.
    pub async fn set_len(&self, len: u64) -> io::Result<()> {
        self.on_pool(move |file| file.set_len(len)).await
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
    /// a duplicate of the file's descriptor.
    async fn on_pool<T, F>(&self, call: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&File) -> io::Result<T> + Send + 'static,
    {
        let mut this = self;
        core::future::poll_fn(|cx| Pin::new(&mut this).poll_flush(cx)).await?;
        let file = self.get_ref().try_clone()?;
        blocking(move || call(&file)).await
    }
}
