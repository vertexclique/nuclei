//! A file's calls in flight and how they are made: a job of the blocking
//! pool, or an operation on the ring of the shard whose thread asks; and the
//! buffers of the pool they read into and write from.

use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::future::Future;
use std::io;
use std::sync::Arc;

use super::FileIo;
use crate::buf::{PoolBuf, with_pool};
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

pub(super) struct Job<T> {
    pub(super) task: Work<io::Result<T>>,
    /// Where it reads, and the epoch it read in.
    pub(super) at: u64,
    pub(super) epoch: u64,
}

/// A read job into a task's own buffer, which comes back with the result.
pub(super) struct LentRead {
    pub(super) task: Work<(io::Result<usize>, Vec<u8>)>,
    pub(super) at: u64,
    pub(super) epoch: u64,
    /// What the buffer held before: the read's bytes come after.
    pub(super) from: usize,
}

/// The write in flight.
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

/// A buffer of this thread's pool, or the file waits for one.
pub(super) fn buffer(len: usize, after: &mut After) -> Option<PoolBuf> {
    let buf = with_pool(|pool| pool.and_then(|p| p.get(len)));
    if buf.is_none() {
        after.starved = true;
    }
    buf
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

    pub(super) fn submit_read(&self, buf: PoolBuf, at: u64, epoch: u64) -> Job<PoolBuf> {
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::read(&self.fd, at, buf) {
                Ok(job) => {
                    return Job {
                        task: Work::Ring(job),
                        at,
                        epoch,
                    };
                }
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = Arc::clone(&self.fd);
        let mut buf = buf;
        let task = crate::runtime::spawn_blocking(move || {
            let n = sys::pread(file_raw(&fd), buf.spare(), at)?;
            buf.set_filled(n);
            Ok(buf)
        });
        Job {
            task: Work::Pool(task),
            at,
            epoch,
        }
    }

    pub(super) fn submit_write(&self, buf: PoolBuf, at: u64) -> Writing {
        if self.append {
            let fd = Arc::clone(&self.fd);
            return Writing::Append(Work::Pool(crate::runtime::spawn_blocking(move || {
                sys::append_all(file_raw(&fd), buf.filled())
            })));
        }
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::write(&self.fd, at, buf) {
                Ok(job) => return Writing::Pool(Work::Ring(job)),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = Arc::clone(&self.fd);
        Writing::Pool(Work::Pool(crate::runtime::spawn_blocking(move || {
            sys::pwrite_all(file_raw(&fd), buf.filled(), at)
        })))
    }

    pub(super) fn submit_read_owned(&self, buf: Vec<u8>, at: u64, epoch: u64) -> LentRead {
        let from = buf.len();
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::read_lent(&self.fd, at, buf) {
                Ok(job) => {
                    return LentRead {
                        task: Work::Ring(job),
                        at,
                        epoch,
                        from,
                    };
                }
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = Arc::clone(&self.fd);
        let mut buf = buf;
        let task = crate::runtime::spawn_blocking(move || {
            let result = sys::pread_uninit(file_raw(&fd), buf.spare_capacity_mut(), at);
            if let Ok(n) = result {
                // SAFETY: the kernel wrote the first `n` bytes of the spare
                // capacity.
                unsafe { buf.set_len(buf.len() + n) };
            }
            (result, buf)
        });
        LentRead {
            task: Work::Pool(task),
            at,
            epoch,
            from,
        }
    }

    pub(super) fn submit_write_owned(&self, buf: Vec<u8>, at: u64) -> Writing {
        if self.append {
            let fd = Arc::clone(&self.fd);
            return Writing::AppendLent(Work::Pool(crate::runtime::spawn_blocking(move || {
                (sys::append_all(file_raw(&fd), &buf), buf)
            })));
        }
        #[cfg(ring)]
        let buf = match self.on_ring() {
            true => match crate::fs::ring::write_lent(&self.fd, at, buf) {
                Ok(job) => return Writing::Lent(Work::Ring(job)),
                Err(buf) => buf,
            },
            false => buf,
        };
        let fd = Arc::clone(&self.fd);
        Writing::Lent(Work::Pool(crate::runtime::spawn_blocking(move || {
            (sys::pwrite_all(file_raw(&fd), &buf, at), buf)
        })))
    }
}
