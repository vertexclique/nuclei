//! Changing the space a range of a file uses: reserving it, keeping its size,
//! making it a hole, zeroing it. Such a call is a positional write of the
//! range in every rule of `positional`: it waits for the cursor writes
//! accepted before it and for the positional writes left by their tasks;
//! when its task takes its end the file's generation moves on (a hole and a
//! zeroed range change bytes a read-ahead may hold); and if its future is
//! dropped while it is out, the file keeps it until it landed (`orphans`).
//! On io_uring it is an `IORING_OP_FALLOCATE` of the shard's ring, counted as
//! a write; elsewhere a job of the blocking pool, counted the same way.

use core::future::poll_fn;
use core::task::Waker;
use std::io;
use std::sync::Arc;

use super::FileIo;
use super::positional::Out;
use super::work::Work;
use crate::fs::Allocate;
use crate::fs::call::{self, FileOutcome};
use crate::io::sys;

/// The end of the `len` bytes from `offset`.
///
/// # Errors
///
/// `InvalidInput` when the end is past the largest offset a file can have.
pub(crate) fn range_end(offset: u64, len: u64) -> io::Result<u64> {
    offset
        .checked_add(len)
        .filter(|end| i64::try_from(*end).is_ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the range ends past the largest offset a file can have",
            )
        })
}

impl FileIo {
    pub(crate) async fn allocate(
        self: &Arc<Self>,
        offset: u64,
        len: u64,
        mode: Allocate,
    ) -> io::Result<()> {
        range_end(offset, len)?;
        poll_fn(|cx| self.poll_ordered(cx)).await;
        Out {
            io: self,
            work: Some(self.submit_allocate(offset, len, mode, Waker::noop())),
        }
        .finish()
        .await
    }

    /// The change of the range, on the ring or the pool; a call the pool
    /// makes wakes `waker` when it ends.
    fn submit_allocate(
        &self,
        offset: u64,
        len: u64,
        mode: Allocate,
        waker: &Waker,
    ) -> Work<io::Result<()>> {
        #[cfg(ring)]
        if self.on_ring()
            && let Ok(job) = crate::fs::ring::allocate(&self.fd, offset, len, mode)
        {
            return Work::Slot(job);
        }
        let fd = self.fd.clone();
        Work::Slot(call::write_on_pool(
            move || FileOutcome::Write(sys::allocate(fd.raw(), offset, len, mode)),
            call::as_write,
            waker,
        ))
    }
}
