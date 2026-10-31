//! Copying a range of one file into another. The bytes move in chunks, one
//! job of the blocking pool each, so no job holds a pool thread for the
//! whole copy: on Linux `copy_file_range(2)` of up to [`COPY_CHUNK`] bytes,
//! and through a buffer of the pool, one buffer a job, where the kernel
//! cannot (`ENOSYS`, `EXDEV`, `EOPNOTSUPP`, `EPERM`, `EINVAL`, `EBADF`, before
//! any byte was copied, as std's copy does) or the system has no such call.
//!
//! The destination's side is a positional write in every rule of
//! `positional`: the copy first waits for both files' writes accepted before
//! it; when its task takes a chunk's end the destination's generation moves
//! on; and a future dropped while a chunk is out leaves that chunk to the
//! destination (`orphans`), which later calls wait for. The chunks run one
//! after another, so a dropped copy stops after the chunk that was out.
//! Both ranges are checked against each other before any byte moves, since a
//! chunked copy of overlapping ranges of one file would read what it wrote.

use core::future::poll_fn;
use core::task::Waker;
use std::io;
use std::sync::Arc;

use super::FileIo;
use super::positional::Out;
use super::space::range_end;
use super::work::Work;
use crate::buf::PoolBuf;
use crate::fs::call::{self, FileOutcome};
use crate::io::raw::Raw;
use crate::io::sys;

/// The most bytes one job of a copy asks the kernel to move.
pub(crate) const COPY_CHUNK: u64 = 16 << 20;

impl FileIo {
    pub(crate) async fn copy_range_to(
        self: &Arc<Self>,
        to: &Arc<FileIo>,
        from_offset: u64,
        to_offset: u64,
        len: u64,
    ) -> io::Result<u64> {
        let from_end = range_end(from_offset, len)?;
        let to_end = range_end(to_offset, len)?;
        if from_offset < to_end
            && to_offset < from_end
            && sys::same_file(self.fd.raw(), to.fd.raw())?
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the ranges overlap in one file",
            ));
        }
        poll_fn(|cx| self.poll_ordered(cx)).await;
        poll_fn(|cx| to.poll_ordered(cx)).await;
        let mut done = 0;
        let mut in_kernel = sys::COPIES_IN_KERNEL;
        while done < len {
            let want = (len - done).min(COPY_CHUNK);
            let (at, put) = (from_offset + done, to_offset + done);
            let step = match in_kernel {
                true => self.kernel_chunk(to, at, put, want).await,
                false => self.buffered_chunk(to, at, put, want).await,
            };
            match step {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(err) if done == 0 && in_kernel && sys::kernel_copy_unavailable(&err) => {
                    in_kernel = false;
                }
                Err(err) if done == 0 => return Err(err),
                // The bytes copied are the answer; the next call meets the error again.
                Err(_) => break,
            }
        }
        Ok(done)
    }

    /// One chunk of up to `want` bytes, in the kernel.
    async fn kernel_chunk(
        &self,
        to: &Arc<FileIo>,
        from_offset: u64,
        to_offset: u64,
        want: u64,
    ) -> io::Result<u64> {
        let (src, dst) = (self.fd.clone(), to.fd.clone());
        let work = Work::Slot(call::write_on_pool(
            move || {
                FileOutcome::Copied(
                    sys::copy_file_range(
                        src.raw(),
                        from_offset,
                        dst.raw(),
                        to_offset,
                        usize::try_from(want).unwrap_or(usize::MAX),
                    )
                    .map(|n| n as u64),
                )
            },
            call::as_copied,
            Waker::noop(),
        ));
        Out {
            io: to,
            work: Some(work),
        }
        .finish()
        .await
    }

    /// One chunk of up to a buffer of the pool, read and written by a job.
    async fn buffered_chunk(
        &self,
        to: &Arc<FileIo>,
        from_offset: u64,
        to_offset: u64,
        want: u64,
    ) -> io::Result<u64> {
        let (buf, from, n) = self
            .bounce_for(usize::try_from(want).unwrap_or(usize::MAX), 0)
            .await;
        let (src, dst) = (self.fd.clone(), to.fd.clone());
        let work = Work::Slot(call::write_on_pool(
            move || {
                FileOutcome::Copied(through_buffer(
                    (src.raw(), from_offset),
                    (dst.raw(), to_offset),
                    buf,
                    (from, n),
                ))
            },
            call::as_copied,
            Waker::noop(),
        ));
        Out {
            io: to,
            work: Some(work),
        }
        .finish()
        .await
    }
}

/// Reads up to `n` bytes of `buf` from `from` at the offset of `src`, writes
/// them at the offset of `dst`, and answers how many.
fn through_buffer(
    (src, src_at): (Raw, u64),
    (dst, dst_at): (Raw, u64),
    mut buf: PoolBuf,
    (from, n): (usize, usize),
) -> io::Result<u64> {
    let end = from.saturating_add(n).min(buf.capacity());
    let room = buf.spare().get_mut(from..end).unwrap_or_default();
    let got = sys::pread(src, room, src_at)?;
    buf.set_filled_at(from, got);
    sys::pwrite_all(dst, buf.filled(), dst_at)?;
    Ok(got as u64)
}
