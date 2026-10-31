//! Reads and writes at offsets of the caller's own: [`Handle::read_at`],
//! [`Handle::write_at`] and their forms. They never read or move the
//! handle's cursor or its epoch, any number of them run at once on one
//! handle, and they keep these rules with the calls at the cursor:
//!
//! - A positional call first waits for the cursor writes accepted before it
//!   (the write in flight and the write-behind) and for the positional
//!   writes left by their tasks: a positional write is never lost under, or
//!   reordered with, a write that came before it, and a positional read sees
//!   those writes.
//! - When a positional write's task takes its end, the file's generation
//!   moves on: a cursor read made after it sees what it wrote, since a read
//!   job or bytes read ahead count only in the generation they began in. A
//!   positional write still out while a cursor read is taken races it, as
//!   `pwrite` and `pread` from two threads race.
//! - A positional write whose future is dropped while it is out still lands;
//!   the file keeps it until it did (`orphans`).
//!
//! On io_uring each is a read or a write of the shard's ring at its offset;
//! elsewhere a `pread` or `pwrite` on the blocking pool (on Windows a
//! `ReadFile` or `WriteFile` at the offset its `OVERLAPPED` names).
//! `proofs/tla/lent/LentFile.tla` (scenario "positional") checks the rules.

use crate::buf::owned::{IoBuf, IoBufMut, OwnedBuf};
use core::future::poll_fn;
use core::task::{Context, Poll, Waker};
use std::fs::File as StdFile;
use std::io;
use std::sync::Arc;

use super::FileIo;
use super::orphans::Orphan;
use super::work::{Ended, Work, bounce};
use crate::io::Handle;
use crate::io::side::After;

impl FileIo {
    /// Ready once every cursor write accepted before the call, and every
    /// positional write left by its task, landed.
    fn poll_ordered(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<()> {
        if !self.has_unsent() {
            return Poll::Ready(());
        }
        self.with_state(cx, |st, io, waker, after| st.poll_writes(io, waker, after))
    }

    /// A buffer of the pool for the bytes of a task's memory at `addr`, and
    /// where in it and how many of `len` the call moves (see `work::bounce`).
    async fn bounce_for(&self, len: usize, addr: usize) -> (crate::buf::PoolBuf, usize, usize) {
        poll_fn(|cx| match bounce(len, addr, self.direct, cx.waker()) {
            Some(got) => Poll::Ready(got),
            None => Poll::Pending,
        })
        .await
    }

    pub(crate) async fn read_at(self: &Arc<Self>, out: &mut [u8], at: u64) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        poll_fn(|cx| self.poll_ordered(cx)).await;
        let (buf, from, n) = self.bounce_for(out.len(), out.as_ptr() as usize).await;
        let mut job = self.submit_read(buf, from, n, at);
        let buf = poll_fn(|cx| job.poll(cx.waker())).await?;
        Ok(buf.peek_into(out))
    }

    pub(crate) async fn write_at(self: &Arc<Self>, bytes: &[u8], at: u64) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        poll_fn(|cx| self.poll_ordered(cx)).await;
        let (mut buf, from, n) = self.bounce_for(bytes.len(), bytes.as_ptr() as usize).await;
        buf.fill_at(from, &bytes[..n]);
        let mut out = Out {
            io: self,
            work: Some(self.submit_pwrite(buf, at, Waker::noop())),
        };
        poll_fn(|cx| out.poll(cx)).await.map(|()| n)
    }

    pub(crate) async fn read_at_owned<B>(
        self: &Arc<Self>,
        buf: B,
        at: u64,
    ) -> (io::Result<usize>, B)
    where
        B: IoBufMut,
    {
        let buf = OwnedBuf::filled(buf);
        if buf.len() == buf.capacity() {
            return (Ok(0), buf.into_filled());
        }
        poll_fn(|cx| self.poll_ordered(cx)).await;
        let mut job = self.submit_read_owned(buf, at);
        let (result, buf) = poll_fn(|cx| job.poll(cx.waker())).await;
        (result, buf.into_filled())
    }

    pub(crate) async fn write_at_owned<B>(
        self: &Arc<Self>,
        buf: B,
        at: u64,
    ) -> (io::Result<usize>, B)
    where
        B: IoBuf,
    {
        let buf = OwnedBuf::written(buf);
        if buf.is_empty() {
            return (Ok(0), buf.into_written());
        }
        poll_fn(|cx| self.poll_ordered(cx)).await;
        let len = buf.len();
        let mut out = Out {
            io: self,
            work: Some(self.submit_pwrite_owned(buf, at, Waker::noop())),
        };
        let (result, buf) = poll_fn(|cx| out.poll(cx)).await;
        (result.map(|()| len), buf.into_written())
    }

    /// Leaves `orphan`, a positional write whose task went away, to the
    /// file state, which keeps everything after it waiting until it landed.
    fn orphan(self: &Arc<Self>, orphan: Orphan) {
        self.orphans.push(orphan);
        let mut after = After::default();
        self.state.defer(
            super::deferred::ORPHAN,
            &mut after,
            &super::FileHooks { io: self },
        );
        // A call after this one looks at the state: its holder keeps the
        // flag up while the write is out.
        self.unsent
            .store(true, core::sync::atomic::Ordering::Release);
        after.run();
    }
}

/// A positional write out, whose task takes its end. Dropped first, the
/// write is left to the file.
struct Out<'a, T>
where
    Work<T>: Into<Orphan>,
{
    io: &'a Arc<FileIo>,
    work: Option<Work<T>>,
}

impl<T> Out<'_, T>
where
    T: Ended,
    Work<T>: Into<Orphan>,
{
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<T> {
        let Some(work) = self.work.as_mut() else {
            return Poll::Pending;
        };
        let end = core::task::ready!(work.poll(cx.waker()));
        self.work = None;
        // The file may hold what it wrote, failed or not: reads of the file
        // before this, read ahead or still out, are of an older generation.
        // Release: pairs with the Acquire of `FileIo::generation`.
        self.io
            .generation
            .fetch_add(1, core::sync::atomic::Ordering::Release);
        Poll::Ready(end)
    }
}

impl<T> Drop for Out<'_, T>
where
    Work<T>: Into<Orphan>,
{
    fn drop(&mut self) {
        if let Some(work) = self.work.take() {
            self.io.orphan(work.into());
        }
    }
}

impl Handle<StdFile> {
    /// Reads at `offset` into `buf` and answers how many bytes came, zero at
    /// the end of the file, as std's `FileExt::read_at` does (on Windows
    /// `seek_read`, after which the system's own position of the file is
    /// past the bytes read, as std documents; the handle's cursor never
    /// moves).
    ///
    /// The handle's cursor and the reads and writes at it are left as they
    /// are, and any number of positional calls may run at once on one
    /// handle. The read sees every write accepted at the cursor before it,
    /// and every positional write that returned before it.
    ///
    /// The bytes come through a buffer of the runtime's pool, at most 64 KiB
    /// a call (a short read, as `read_at` may answer). On a file opened for
    /// direct IO the call reads exactly `buf.len()` bytes at `offset`, into
    /// memory placed in its page as `buf` is, so the kernel refuses (with
    /// `EINVAL`) exactly what it would refuse of `buf` itself.
    /// [`read_at_owned`](Self::read_at_owned) reads into the caller's own
    /// buffer with no copy.
    ///
    /// # Errors
    ///
    /// As the system's positional read: on a stream (a pipe, a terminal) the
    /// file has no offset to read at.
    pub async fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.file()?.read_at(buf, offset).await
    }

    /// Writes `buf` at `offset` and answers how many bytes landed, as std's
    /// `FileExt::write_at` does (on Windows `seek_write`, after which the
    /// system's own position of the file is past the bytes written; the
    /// handle's cursor never moves). On Linux a file opened to append takes
    /// the bytes at its end whatever the offset, as `pwrite(2)` does there.
    ///
    /// The write lands after every write accepted at the cursor before it,
    /// and before any write the handle accepts after it returned; positional
    /// writes running at once land in any order, as `pwrite` from two
    /// threads does. If the future is dropped while the write is out, it
    /// still lands, and every later call of the handle waits for it.
    ///
    /// The bytes go through a buffer of the runtime's pool, at most 64 KiB a
    /// call (a short write, as `write_at` may answer). On a file opened for
    /// direct IO the call writes exactly `buf` at `offset`, from memory
    /// placed in its page as `buf` is, so the kernel refuses (with `EINVAL`)
    /// exactly what it would refuse of `buf` itself.
    /// [`write_at_owned`](Self::write_at_owned) writes from the caller's own
    /// buffer with no copy.
    ///
    /// # Errors
    ///
    /// As the system's positional write.
    pub async fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        self.file()?.write_at(buf, offset).await
    }

    /// Reads exactly `buf.len()` bytes at `offset`, as std's
    /// `FileExt::read_exact_at` does, with [`read_at`](Self::read_at).
    ///
    /// # Errors
    ///
    /// `UnexpectedEof` when the file ends first (the bytes read so far are in
    /// `buf`), or the first error of a read.
    pub async fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.read_at(buf, offset).await {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ));
                }
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// Writes all of `buf` at `offset`, as std's `FileExt::write_all_at`
    /// does, with [`write_at`](Self::write_at).
    ///
    /// # Errors
    ///
    /// `WriteZero` when a write takes nothing, or the first error of a write.
    pub async fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.write_at(buf, offset).await {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ));
                }
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// Reads at `offset` into the spare capacity of `buf`, after the bytes
    /// it holds, in one call of the system's positional read, and gives
    /// `buf` back with how many bytes came: zero at the end of the file, or
    /// when `buf` has no spare capacity. The bytes are read into `buf`
    /// itself, never copied; on a file opened for direct IO the kernel's
    /// alignment rules apply to `buf`'s own memory, which an
    /// [`AlignedBuf`](crate::io::AlignedBuf) aligns. `buf` is any
    /// [`IoBufMut`](crate::io::IoBufMut). Otherwise as
    /// [`read_at`](Self::read_at).
    pub async fn read_at_owned<B>(&self, buf: B, offset: u64) -> (io::Result<usize>, B)
    where
        B: IoBufMut,
    {
        match self.file() {
            Ok(file) => file.read_at_owned(buf, offset).await,
            Err(err) => (Err(err), buf),
        }
    }

    /// Writes all of `buf` at `offset` and gives it back with its length
    /// once the bytes landed. They are written from `buf` itself, never
    /// copied; on a file opened for direct IO the kernel's alignment rules
    /// apply to `buf`'s own memory, which an
    /// [`AlignedBuf`](crate::io::AlignedBuf) aligns. If the future is dropped
    /// while the write is out, it still lands, `buf` going with it. `buf` is
    /// any [`IoBuf`](crate::io::IoBuf). Otherwise as
    /// [`write_at`](Self::write_at).
    pub async fn write_at_owned<B>(&self, buf: B, offset: u64) -> (io::Result<usize>, B)
    where
        B: IoBuf,
    {
        match self.file() {
            Ok(file) => file.write_at_owned(buf, offset).await,
            Err(err) => (Err(err), buf),
        }
    }
}
