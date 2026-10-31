//! A file's calls in flight and how they are made: a job of the blocking
//! pool, or an operation on the ring of the shard whose thread asks; and the
//! buffers of the pool they read into and write from.
//!
//! Every call names exactly the bytes it moves: a read the range of its
//! buffer it fills, a write the bytes its buffer holds. A call into a buffer
//! of the pool for a task's own memory (a positional call, or any call on a
//! file opened for direct IO) mirrors that memory's place in its page, so
//! the kernel finds the alignment the task's memory has.

use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::future::Future;
use std::io;

use super::FileIo;
use crate::buf::owned::OwnedBuf;
use crate::buf::{LARGEST, PoolBuf, with_pool};
use crate::fs::call::{self, FileOutcome, SlotJob};
use crate::io::raw::Raw;
use crate::io::side::After;
use crate::io::sys;
use crate::rt::task::Task;

/// The page a task's memory is mirrored within, for the kernel's alignment
/// rules of direct IO (which never ask for more than a page of memory).
pub(super) const PAGE: usize = 4096;

/// A file's call in flight: a job of the blocking pool, or a call that ends
/// in a slot (an operation on the ring of the shard whose thread issued it,
/// or a write the pool makes).
pub(super) enum Work<T> {
    Pool(Task<T>),
    Slot(SlotJob<T>),
}

impl<T: Ended> Work<T> {
    /// The call's end once it came, its error counted for `stats`.
    #[inline]
    pub(super) fn poll(&mut self, waker: &Waker) -> Poll<T> {
        let end = match self {
            Work::Pool(task) => poll_task(task, waker),
            Work::Slot(job) => job.poll(waker),
        };
        if let Poll::Ready(end) = &end
            && let Some(err) = end.error()
        {
            crate::rt::counters::io_error(err);
        }
        end
    }
}

impl<T> Work<T> {
    /// Lets the call run to its end with nobody to take it: a job of the
    /// pool is detached (dropped, one not started would be cancelled), a
    /// call that ends in a slot goes on by itself.
    pub(super) fn let_go(self) {
        match self {
            Work::Pool(task) => task.detach(),
            Work::Slot(job) => drop(job),
        }
    }
}

/// The end of a file's call: its result, alone or with the task's buffer.
pub(super) trait Ended {
    fn error(&self) -> Option<&io::Error>;
}

impl<T> Ended for io::Result<T> {
    fn error(&self) -> Option<&io::Error> {
        self.as_ref().err()
    }
}

impl<T> Ended for (io::Result<T>, OwnedBuf) {
    fn error(&self) -> Option<&io::Error> {
        self.0.as_ref().err()
    }
}

/// A read job at the cursor, into a buffer of the pool.
pub(super) struct Job<T> {
    pub(super) task: Work<io::Result<T>>,
    /// Where it reads, the epoch and the generation it read in.
    pub(super) at: u64,
    pub(super) epoch: u64,
    pub(super) generation: u64,
}

/// A read job at the cursor into a task's own buffer, which comes back
/// with the result.
pub(super) struct LentRead {
    pub(super) task: Work<(io::Result<usize>, OwnedBuf)>,
    pub(super) at: u64,
    pub(super) epoch: u64,
    pub(super) generation: u64,
    /// What the buffer held before: the read's bytes come after.
    pub(super) from: usize,
}

/// The cursor write in flight.
pub(super) enum Writing {
    /// From a buffer of the pool.
    Pool(Work<io::Result<()>>),
    /// From a task's own buffer, which comes back with the result.
    Lent(Work<(io::Result<()>, OwnedBuf)>),
    /// At the end of a file opened to append, ending with the file's offset
    /// after it.
    Append(Work<io::Result<u64>>),
    /// As `Append`, from a task's own buffer.
    AppendLent(Work<(io::Result<u64>, OwnedBuf)>),
}

/// A write from a task's own buffer, from its submission until its task
/// takes the buffer back.
#[derive(Default)]
pub(super) enum Lent {
    #[default]
    None,
    /// In flight, its task waiting.
    Out,
    /// Done, for its task to take.
    Done(io::Result<()>, OwnedBuf),
    /// In flight, its task gone: the buffer is dropped when it lands.
    Orphan,
}

/// Writes all of a task's `buf` from its `from`th byte at `at` on, every
/// buffer of a vectored write in turn.
pub(crate) fn pwrite_owned(fd: Raw, buf: &mut OwnedBuf, from: usize, at: u64) -> io::Result<()> {
    let Some(bufs) = buf.vectored_mut() else {
        return sys::pwrite_all(fd, buf.rest_from(from), at);
    };
    let (len, mut done) = (bufs.len(), from);
    while done < len {
        let n = sys::pwritev(fd, bufs.write_iovecs(done), at + (done - from) as u64)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        done += n;
    }
    Ok(())
}

/// Reads at `at` into the spare capacity of a task's `buf`, after the bytes
/// it holds (of a vectored read, into each buffer's room in order, in one
/// call), and answers how many bytes came.
pub(crate) fn pread_owned(fd: Raw, buf: &mut OwnedBuf, at: u64) -> io::Result<usize> {
    let result = match buf.vectored_mut() {
        Some(bufs) => sys::preadv(fd, bufs.read_iovecs(), at),
        None => sys::pread_uninit(fd, buf.spare_capacity_mut(), at),
    };
    if let Ok(n) = result {
        // SAFETY: the kernel wrote the first `n` bytes of the spare capacity
        // (of a vectored read, its rooms in order).
        unsafe { buf.set_len(buf.len() + n) };
    }
    result
}

/// Writes all of a task's `buf` at the end of a file opened to append, and
/// answers where the file's offset is after it.
fn append_owned(fd: Raw, buf: &mut OwnedBuf) -> io::Result<u64> {
    let Some(bufs) = buf.vectored_mut() else {
        return sys::append_all(fd, buf);
    };
    #[cfg(unix)]
    {
        // One `writev` a call: each lands whole at the file's end.
        let (len, mut done) = (bufs.len(), 0);
        while done < len {
            let n = sys::writev(fd, bufs.write_iovecs(done))?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            done += n;
        }
        sys::seek(fd, None)
    }
    // A file on Windows has no vectored write: a write each, in order.
    #[cfg(windows)]
    {
        let (len, mut done, mut end) = (bufs.len(), 0, 0);
        while done < len {
            let rest = bufs.rest_from(done);
            end = sys::append_all(fd, rest)?;
            done += rest.len();
        }
        Ok(end)
    }
}

#[inline]
pub(super) fn poll_task<T>(task: &mut Task<T>, waker: &Waker) -> Poll<T> {
    Pin::new(task).poll(&mut Context::from_waker(waker))
}

/// A buffer of this thread's pool of at least `len` bytes (or of its largest
/// class), or the file waits for one.
#[inline]
pub(super) fn buffer(len: usize, after: &mut After) -> Option<PoolBuf> {
    let buf = with_pool(|pool| pool.and_then(|p| p.get(len)));
    if buf.is_none() {
        after.starved = true;
    }
    buf
}

/// The most bytes one call on a file opened for direct IO moves through a
/// buffer of the pool: the largest power of two within a quarter of what
/// the pool may hold, so the buffer always fits beside others.
fn direct_bound(cap: usize) -> usize {
    match cap / 4 {
        0 => LARGEST,
        quarter => (1usize << quarter.ilog2()).max(LARGEST),
    }
}

/// How many of `len` bytes the first call takes when a call may move at most
/// `bound`: all of them when they fit; else the remainder past a multiple of
/// `bound` first, so a length the kernel refuses is refused at once, and
/// every later call moves `bound` bytes from an offset as aligned as the
/// caller's.
fn first_part(len: usize, bound: usize) -> usize {
    if len <= bound {
        return len;
    }
    match len % bound {
        0 => bound,
        rest => rest,
    }
}

/// A buffer of this thread's pool for a call of up to `len` bytes of a
/// task's memory at `addr`, and how many bytes the call moves: with
/// `direct`, the bytes start in the buffer where `addr` lies in its page
/// (see [`PAGE`]) and the call moves all `len` bytes (or a first part of
/// them past what one buffer may hold); else from the buffer's start, up to
/// a buffer of the pool's largest class. `None` while the pool has no room:
/// `waker` is woken once a buffer comes back.
pub(crate) fn bounce(
    len: usize,
    addr: usize,
    direct: bool,
    waker: &Waker,
) -> Option<(PoolBuf, usize, usize)> {
    let shift = if direct { addr % PAGE } else { 0 };
    with_pool(|pool| {
        let pool = pool?;
        let want = if direct {
            first_part(len, direct_bound(pool.cap()))
        } else {
            len.min(LARGEST)
        };
        // Classes below a page are aligned to a cache line only: a mirrored
        // call takes a page-aligned one.
        let size = if direct {
            (want + shift).max(PAGE)
        } else {
            want
        };
        let buf = if pool.could_hold_whole(size) {
            pool.get_whole(size)
        } else {
            pool.get(size)
        };
        let Some(buf) = buf else {
            pool.wait_for_return(waker.clone());
            return None;
        };
        // vertexia: a pool too small to hold a page mirrors nothing; such a
        // pool cannot serve direct IO's alignment anyway.
        let shift = if buf.capacity() > shift { shift } else { 0 };
        let n = want.min(buf.capacity() - shift);
        Some((buf, shift, n))
    })
}

impl FileIo {
    /// Whether this thread's calls go to its shard's ring.
    #[cfg(ring)]
    #[inline]
    pub(super) fn on_ring(&self) -> bool {
        #[cfg(test)]
        if self.on_pool.load(core::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        true
    }

    /// A read at `at` into `len` bytes of `buf` from `from`.
    #[inline]
    pub(super) fn submit_read(
        &self,
        buf: PoolBuf,
        from: usize,
        len: usize,
        at: u64,
    ) -> Work<io::Result<PoolBuf>> {
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::read(&self.fd, at, buf, from, len) {
                Ok(job) => return Work::Slot(job),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = self.fd.clone();
        let mut buf = buf;
        Work::Pool(crate::runtime::spawn_blocking(move || {
            let end = (from + len).min(buf.capacity());
            let n = sys::pread(fd.raw(), &mut buf.spare()[from..end], at)?;
            buf.set_filled_at(from, n);
            Ok(buf)
        }))
    }

    /// A write of the bytes `buf` holds at `at`, whole: for a positional
    /// write, and the cursor's on a file not opened to append. A write the
    /// pool makes wakes `waker` when it ends.
    #[inline]
    pub(super) fn submit_pwrite(
        &self,
        buf: PoolBuf,
        at: u64,
        waker: &Waker,
    ) -> Work<io::Result<()>> {
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::write(&self.fd, at, buf) {
                Ok(job) => return Work::Slot(job),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = self.fd.clone();
        Work::Slot(call::write_on_pool(
            move || FileOutcome::Write(sys::pwrite_all(fd.raw(), buf.filled(), at)),
            call::as_write,
            waker,
        ))
    }

    /// The cursor's write of `buf` at `at`, or at the end of a file opened
    /// to append; a write the pool makes wakes `waker` when it ends.
    #[inline]
    pub(super) fn submit_write(&self, buf: PoolBuf, at: u64, waker: &Waker) -> Writing {
        if self.append {
            let fd = self.fd.clone();
            return Writing::Append(Work::Slot(call::write_on_pool(
                move || FileOutcome::Append(sys::append_all(fd.raw(), buf.filled())),
                call::as_append,
                waker,
            )));
        }
        Writing::Pool(self.submit_pwrite(buf, at, waker))
    }

    /// A read at `at` into the spare capacity of a task's `buf`, after the
    /// bytes it holds.
    #[inline]
    pub(super) fn submit_read_owned(
        &self,
        buf: OwnedBuf,
        at: u64,
    ) -> Work<(io::Result<usize>, OwnedBuf)> {
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::read_lent(&self.fd, at, buf) {
                Ok(job) => return Work::Slot(job),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = self.fd.clone();
        let mut buf = buf;
        Work::Pool(crate::runtime::spawn_blocking(move || {
            let result = pread_owned(fd.raw(), &mut buf, at);
            (result, buf)
        }))
    }

    /// A write of all of a task's `buf` at `at`: for a positional write, and
    /// the cursor's on a file not opened to append. A write the pool makes
    /// wakes `waker` when it ends.
    #[inline]
    pub(super) fn submit_pwrite_owned(
        &self,
        buf: OwnedBuf,
        at: u64,
        waker: &Waker,
    ) -> Work<(io::Result<()>, OwnedBuf)> {
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::write_lent(&self.fd, at, buf) {
                Ok(job) => return Work::Slot(job),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = self.fd.clone();
        Work::Slot(call::write_on_pool(
            move || {
                let mut buf = buf;
                let result = pwrite_owned(fd.raw(), &mut buf, 0, at);
                FileOutcome::WriteLent(result, buf)
            },
            call::as_write_lent,
            waker,
        ))
    }

    /// The cursor's write of all of a task's `buf` at `at`, or at the end of
    /// a file opened to append; a write the pool makes wakes `waker` when it
    /// ends.
    #[inline]
    pub(super) fn submit_write_owned(&self, buf: OwnedBuf, at: u64, waker: &Waker) -> Writing {
        if self.append {
            let fd = self.fd.clone();
            return Writing::AppendLent(Work::Slot(call::write_on_pool(
                move || {
                    let mut buf = buf;
                    let result = append_owned(fd.raw(), &mut buf);
                    FileOutcome::AppendLent(result, buf)
                },
                call::as_append_lent,
                waker,
            )));
        }
        Writing::Lent(self.submit_pwrite_owned(buf, at, waker))
    }
}
