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
//!
//! A sync of a range of the file (`sync_range`) is the same call with
//! another kind: on Linux `sync_file_range(2)` (an `IORING_OP_SYNC_FILE_RANGE`
//! on the ring, for a range its length field holds), which writes the range's
//! pages back and waits for them but flushes no metadata and no device cache;
//! elsewhere the sync of the file's data. It waits for the writes it covers as
//! the others do, and its failure is the handle's sticky one: the kernel
//! reports a write-back error it meets to that call alone.

use core::future::poll_fn;
use std::io;
use std::sync::Arc;

use super::FileIo;
#[cfg(ring)]
use super::work::Work;
use crate::io::source::Failure;
use crate::io::source::requests::Lease;

/// What a sync writes through.
#[derive(Clone, Copy)]
pub(crate) enum SyncKind {
    /// The data and all the metadata.
    All,
    /// The data, and the metadata a read needs.
    Data,
    /// The pages of `len` bytes from `offset` (zero: to the file's end).
    Range {
        // Only Linux's call reads the range: elsewhere the file's data is synced.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        offset: u64,
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        len: u64,
    },
}

impl FileIo {
    /// Writes what `kind` says through, once every write it covers landed.
    pub(crate) async fn sync(self: &Arc<Self>, kind: SyncKind) -> io::Result<()> {
        poll_fn(|cx| self.poll_flush(cx)).await?;
        if let Some(failed) = self.sync_failed.get() {
            return Err(failed.error());
        }
        let result = self.sync_call(kind).await;
        if let Err(err) = &result {
            let _ = self.sync_failed.set(Failure::of(err));
        }
        // A sync that ended after another failed says nothing either.
        result.and_then(|()| self.sync_failed.get().map_or(Ok(()), |f| Err(f.error())))
    }

    async fn sync_call(&self, kind: SyncKind) -> io::Result<()> {
        #[cfg(ring)]
        if self.on_ring() {
            let job = match kind {
                SyncKind::All => crate::fs::ring::sync(&self.fd, false),
                SyncKind::Data => crate::fs::ring::sync(&self.fd, true),
                // The entry's length field is 32 bits: a longer range goes to the pool.
                SyncKind::Range { offset, len } => match u32::try_from(len) {
                    Ok(len) => crate::fs::ring::sync_range(&self.fd, offset, len),
                    Err(_) => Err(()),
                },
            };
            if let Ok(job) = job {
                let mut work = Work::Slot(job);
                return poll_fn(|cx| work.poll(cx.waker())).await;
            }
        }
        let fd = self.fd.clone();
        crate::rt::blocking::call(move || sync_on(&fd, kind)).await
    }
}

/// std's sync of the file `fd` (`fdatasync` for `Data`, else `fsync`;
/// `F_FULLFSYNC` on Apple's systems; `FlushFileBuffers` on Windows), or the
/// write-back of a range.
pub(crate) fn sync_on(fd: &Lease, kind: SyncKind) -> io::Result<()> {
    sync_file(&fd.file(), kind)
}

/// As [`sync_on`], on a file at hand. A range is written back and waited for
/// by `sync_file_range(2)` on Linux, which a stream refuses as the kernel
/// does; elsewhere it is the sync of the file's data, which covers it.
pub(crate) fn sync_file(file: &std::fs::File, kind: SyncKind) -> io::Result<()> {
    match kind {
        SyncKind::All => file.sync_all(),
        #[cfg(not(target_os = "linux"))]
        SyncKind::Data | SyncKind::Range { .. } => file.sync_data(),
        #[cfg(target_os = "linux")]
        SyncKind::Data => file.sync_data(),
        #[cfg(target_os = "linux")]
        SyncKind::Range { offset, len } => {
            use std::os::fd::AsRawFd;
            crate::io::sys::sync_file_range(file.as_raw_fd(), offset, len)
        }
    }
}
