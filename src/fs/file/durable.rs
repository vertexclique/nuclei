//! Syncing a file: on io_uring an `IORING_OP_FSYNC` of the shard's ring
//! (with `IORING_FSYNC_DATASYNC` for a sync of the data), elsewhere a job of
//! the blocking pool calling std's own sync; either way on the jobs' copy of
//! the descriptor.
//!
//! A sync covers every write accepted at the cursor before it was called and
//! every positional write that returned before it: it waits until the
//! write-behind went out and every write in flight (and every positional
//! write left by its task) landed, then makes its call. Waiting for their
//! ends orders the sync after them on every backend; the ring's
//! `IOSQE_IO_DRAIN` would not (a write its shard handed to the blocking pool
//! is not the ring's), and would hold the sync behind every other operation
//! of the ring, a multishot receive among them, which may never end
//! (`proofs/tla/lent/LentFile.tla`, scenario "sync", `MF_sync_drain`).
//!
//! A write that failed is reported by the sync, as by a flush (and then
//! forgotten, as a flush forgets it). A failed sync is reported, and every
//! later sync of the handle fails with the same error without a call: the
//! kernel reports a write-back error to one sync only, so a later sync that
//! succeeded would say nothing of the data that did not reach the device.

use core::future::poll_fn;
use core::mem::ManuallyDrop;
use std::io;
use std::sync::Arc;

use super::FileIo;
#[cfg(ring)]
use super::work::Work;
use crate::io::raw::OwnedFile;
use crate::io::source::Failure;

impl FileIo {
    /// Writes the file's data (with `data_only`, and the metadata a read
    /// needs; else all its metadata) through to the device, once every write
    /// it covers landed.
    pub(crate) async fn sync(self: &Arc<Self>, data_only: bool) -> io::Result<()> {
        poll_fn(|cx| self.poll_flush(cx)).await?;
        if let Some(failed) = self.sync_failed.get() {
            return Err(failed.error());
        }
        let result = self.sync_call(data_only).await;
        if let Err(err) = &result {
            let _ = self.sync_failed.set(Failure::of(err));
        }
        // A sync that ended after another failed says nothing either.
        result.and_then(|()| self.sync_failed.get().map_or(Ok(()), |f| Err(f.error())))
    }

    async fn sync_call(&self, data_only: bool) -> io::Result<()> {
        #[cfg(ring)]
        if self.on_ring()
            && let Ok(job) = crate::fs::ring::sync(&self.fd, data_only)
        {
            let mut work = Work::Ring(job);
            return poll_fn(|cx| work.poll(cx.waker())).await;
        }
        let fd = Arc::clone(&self.fd);
        crate::rt::blocking::call(move || sync_on(&fd, data_only)).await
    }
}

/// std's sync of the file `fd` (`fdatasync` with `data_only`, else `fsync`;
/// `F_FULLFSYNC` on Apple's systems; `FlushFileBuffers` on Windows), made
/// on a view of the descriptor that never closes it.
pub(crate) fn sync_on(fd: &OwnedFile, data_only: bool) -> io::Result<()> {
    #[cfg(unix)]
    // SAFETY: `fd` stays open while it is borrowed, and the view is never
    // dropped, so it never closes it.
    let file = ManuallyDrop::new(unsafe {
        <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(std::os::fd::AsRawFd::as_raw_fd(fd))
    });
    #[cfg(windows)]
    // SAFETY: as on unix: the handle stays open while borrowed, and the view
    // never closes it.
    let file = ManuallyDrop::new(unsafe {
        <std::fs::File as std::os::windows::io::FromRawHandle>::from_raw_handle(
            std::os::windows::io::AsRawHandle::as_raw_handle(fd),
        )
    });
    if data_only {
        file.sync_data()
    } else {
        file.sync_all()
    }
}
