//! Syncing a file: on io_uring an `IORING_OP_FSYNC` of the shard's ring
//! (with `IORING_FSYNC_DATASYNC` for a sync of the data), elsewhere a job of
//! the blocking pool calling std's own sync; either way on the handle's own
//! descriptor, leased for the call.
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
use std::io;
use std::sync::Arc;

use super::FileIo;
#[cfg(ring)]
use super::work::Work;
use crate::io::source::Failure;
use crate::io::source::requests::Lease;

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
            let mut work = Work::Slot(job);
            return poll_fn(|cx| work.poll(cx.waker())).await;
        }
        let fd = self.fd.clone();
        crate::rt::blocking::call(move || sync_on(&fd, data_only)).await
    }
}

/// std's sync of the file `fd` (`fdatasync` with `data_only`, else `fsync`;
/// `F_FULLFSYNC` on Apple's systems; `FlushFileBuffers` on Windows).
pub(crate) fn sync_on(fd: &Lease, data_only: bool) -> io::Result<()> {
    let file = fd.file();
    if data_only {
        file.sync_data()
    } else {
        file.sync_all()
    }
}
