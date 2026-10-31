//! Copying a range of one file into another.

use std::fs::File;
use std::io;

use super::file::{COPY_CHUNK, range_end};
use super::ops::blocking;
use crate::io::Handle;
use crate::io::sys;

impl Handle<File> {
    /// Copies up to `len` bytes from this file at `from_offset` to `to` at
    /// `to_offset` and answers how many were copied: fewer only where this
    /// file ends, and zero at its end. Neither file's cursor is used or
    /// moved.
    ///
    /// On Linux the bytes move in the kernel (`copy_file_range(2)`), in jobs
    /// of the blocking pool that each take a chunk of at most 16 MiB, so no
    /// job holds a pool thread for a whole copy. Where the kernel cannot copy
    /// between the two files, or the system has no such call (macOS,
    /// Windows, the other unix), the bytes go through a buffer of the
    /// runtime's pool, one buffer a job.
    ///
    /// The copy waits for the writes accepted before it on both files. For
    /// `to` it is a write: a read at its cursor made after the copy sees the
    /// bytes, and a future dropped while a chunk is out leaves that chunk to
    /// `to`, which every later call of `to` waits for; the chunks after it
    /// are not made. A failure after some bytes were copied is not reported:
    /// the count is, and the next call meets the failure again.
    ///
    /// A file that is not a regular file (a pipe, a terminal) gets the
    /// system's answer to one call.
    ///
    /// # Errors
    ///
    /// `InvalidInput` when the end of either range is past the largest offset
    /// a file can have, or when both handles name one file and the ranges
    /// overlap; otherwise the system's own error, among them that of a handle
    /// that cannot be read or written.
    pub async fn copy_range_to(
        &self,
        to: &Handle<File>,
        from_offset: u64,
        to_offset: u64,
        len: u64,
    ) -> io::Result<u64> {
        if !self.is_regular() || !to.is_regular() {
            range_end(from_offset, len)?;
            range_end(to_offset, len)?;
            let (src, dst) = (self.source().lease()?, to.source().lease()?);
            let want = usize::try_from(len.min(COPY_CHUNK)).unwrap_or(usize::MAX);
            return blocking(move || {
                sys::copy_file_range(src.raw(), from_offset, dst.raw(), to_offset, want)
                    .map(|n| n as u64)
            })
            .await;
        }
        self.file()?
            .copy_range_to(to.file()?, from_offset, to_offset, len)
            .await
    }
}
