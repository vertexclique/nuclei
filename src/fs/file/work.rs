//! A file's calls in flight and how they are made: a job of the blocking
//! pool, or an operation on the ring of the shard whose thread asks; and the
//! buffers of the pool they read into and write from.
//!
//! Every call names exactly the bytes it moves: a read the range of its
//! buffer it fills, a write the bytes its buffer holds.

use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::future::Future;
use std::io;
use std::sync::Arc;

use super::FileIo;
use crate::buf::{LARGEST, PoolBuf, with_pool};
use crate::io::raw::file_raw;
use crate::io::side::After;
use crate::io::sys;
use crate::rt::task::Task;

/// A file's call in flight: a job of the blocking pool, or an operation on
/// the ring of the shard whose thread issued it.
pub(super) enum Work<T> {
    Pool(Task<T>),
    #[cfg(ring)]
    Ring(crate::fs::ring::RingJob<T>),
}

impl<T: Ended> Work<T> {
    /// The call's end once it came, its error counted for `stats`.
    pub(super) fn poll(&mut self, waker: &Waker) -> Poll<T> {
        let end = match self {
            Work::Pool(task) => poll_task(task, waker),
            #[cfg(ring)]
            Work::Ring(job) => job.poll(waker),
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
    /// pool is detached (dropped, one not started would be cancelled), an
    /// operation of the ring goes on by itself.
    pub(super) fn let_go(self) {
        match self {
            Work::Pool(task) => task.detach(),
            #[cfg(ring)]
            Work::Ring(job) => drop(job),
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

impl<T> Ended for (io::Result<T>, Vec<u8>) {
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
    pub(super) task: Work<(io::Result<usize>, Vec<u8>)>,
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
    Lent(Work<(io::Result<()>, Vec<u8>)>),
    /// At the end of a file opened to append, ending with the file's offset
    /// after it.
    Append(Work<io::Result<u64>>),
    /// As `Append`, from a task's own buffer.
    AppendLent(Work<(io::Result<u64>, Vec<u8>)>),
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
    Done(io::Result<()>, Vec<u8>),
    /// In flight, its task gone: the buffer is dropped when it lands.
    Orphan,
}

pub(super) fn poll_task<T>(task: &mut Task<T>, waker: &Waker) -> Poll<T> {
    Pin::new(task).poll(&mut Context::from_waker(waker))
}

/// A buffer of this thread's pool of at least `len` bytes (or of its largest
/// class), or the file waits for one.
pub(super) fn buffer(len: usize, after: &mut After) -> Option<PoolBuf> {
    let buf = with_pool(|pool| pool.and_then(|p| p.get(len)));
    if buf.is_none() {
        after.starved = true;
    }
    buf
}

/// A buffer of this thread's pool for a call of up to `len` bytes of a
/// task's memory, and how many of them the call moves: up to a buffer of the
/// pool's largest class. `None` while the pool has no room: `waker` is woken
/// once a buffer comes back.
pub(super) fn bounce(len: usize, waker: &Waker) -> Option<(PoolBuf, usize)> {
    with_pool(|pool| {
        let pool = pool?;
        let want = len.min(LARGEST);
        let buf = if pool.could_hold_whole(want) {
            pool.get_whole(want)
        } else {
            pool.get(want)
        };
        let Some(buf) = buf else {
            pool.wait_for_return(waker.clone());
            return None;
        };
        let n = want.min(buf.capacity());
        Some((buf, n))
    })
}

impl FileIo {
    /// Whether this thread's calls go to its shard's ring.
    #[cfg(ring)]
    pub(super) fn on_ring(&self) -> bool {
        #[cfg(test)]
        if self.on_pool.load(core::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        true
    }

    /// A read at `at` into `len` bytes of `buf` from `from`.
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
                Ok(job) => return Work::Ring(job),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = Arc::clone(&self.fd);
        let mut buf = buf;
        Work::Pool(crate::runtime::spawn_blocking(move || {
            let end = (from + len).min(buf.capacity());
            let n = sys::pread(file_raw(&fd), &mut buf.spare()[from..end], at)?;
            buf.set_filled_at(from, n);
            Ok(buf)
        }))
    }

    /// A write of the bytes `buf` holds at `at`, whole: for a positional
    /// write, and the cursor's on a file not opened to append.
    pub(super) fn submit_pwrite(&self, buf: PoolBuf, at: u64) -> Work<io::Result<()>> {
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::write(&self.fd, at, buf) {
                Ok(job) => return Work::Ring(job),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = Arc::clone(&self.fd);
        Work::Pool(crate::runtime::spawn_blocking(move || {
            sys::pwrite_all(file_raw(&fd), buf.filled(), at)
        }))
    }

    /// The cursor's write of `buf` at `at`, or at the end of a file opened
    /// to append.
    pub(super) fn submit_write(&self, buf: PoolBuf, at: u64) -> Writing {
        if self.append {
            let fd = Arc::clone(&self.fd);
            return Writing::Append(Work::Pool(crate::runtime::spawn_blocking(move || {
                sys::append_all(file_raw(&fd), buf.filled())
            })));
        }
        Writing::Pool(self.submit_pwrite(buf, at))
    }

    /// A read at `at` into the spare capacity of a task's `buf`, after the
    /// bytes it holds.
    pub(super) fn submit_read_owned(
        &self,
        buf: Vec<u8>,
        at: u64,
    ) -> Work<(io::Result<usize>, Vec<u8>)> {
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::read_lent(&self.fd, at, buf) {
                Ok(job) => return Work::Ring(job),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = Arc::clone(&self.fd);
        let mut buf = buf;
        Work::Pool(crate::runtime::spawn_blocking(move || {
            let result = sys::pread_uninit(file_raw(&fd), buf.spare_capacity_mut(), at);
            if let Ok(n) = result {
                // SAFETY: the kernel wrote the first `n` bytes of the spare
                // capacity.
                unsafe { buf.set_len(buf.len() + n) };
            }
            (result, buf)
        }))
    }

    /// A write of all of a task's `buf` at `at`: for a positional write, and
    /// the cursor's on a file not opened to append.
    pub(super) fn submit_pwrite_owned(
        &self,
        buf: Vec<u8>,
        at: u64,
    ) -> Work<(io::Result<()>, Vec<u8>)> {
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::write_lent(&self.fd, at, buf) {
                Ok(job) => return Work::Ring(job),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = Arc::clone(&self.fd);
        Work::Pool(crate::runtime::spawn_blocking(move || {
            (sys::pwrite_all(file_raw(&fd), &buf, at), buf)
        }))
    }

    /// The cursor's write of all of a task's `buf` at `at`, or at the end of
    /// a file opened to append.
    pub(super) fn submit_write_owned(&self, buf: Vec<u8>, at: u64) -> Writing {
        if self.append {
            let fd = Arc::clone(&self.fd);
            return Writing::AppendLent(Work::Pool(crate::runtime::spawn_blocking(move || {
                (sys::append_all(file_raw(&fd), &buf), buf)
            })));
        }
        Writing::Lent(self.submit_pwrite_owned(buf, at))
    }
}
