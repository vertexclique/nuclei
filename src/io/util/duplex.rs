//! In-memory pipes: [`duplex`] (a pair of connected streams) and [`simplex`]
//! (one pipe, a reading end and a writing end).
//!
//! Each pipe is a ring of bytes with one writer and one reader: the writer
//! copies in and publishes its count with a release store, the reader copies
//! out and publishes its own the same way; each looks at the other's count
//! with an acquire load. A side that finds the ring empty (or full)
//! registers its waker and looks once more before it waits, and each side
//! wakes the other after it moves its count, so no wake is lost. No lock
//! and no allocation after the ring is made.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::sync::Arc;

use futures_io::{AsyncRead, AsyncWrite};

use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicBool, AtomicUsize, Ordering};

/// One pipe: a ring of `capacity` bytes, the counts of bytes written and
/// read since it was made, and the two sides' wakers and ends.
struct Pipe {
    ring: Box<[core::cell::UnsafeCell<u8>]>,
    /// Bytes written since the pipe was made (the writer's alone to move).
    written: AtomicUsize,
    /// Bytes read since the pipe was made (the reader's alone to move).
    read: AtomicUsize,
    reader: AtomicWaker,
    writer: AtomicWaker,
    /// The writing end closed: the reader sees the end once the ring is empty.
    write_closed: AtomicBool,
    /// The reading end is gone: writes fail.
    read_closed: AtomicBool,
}

// SAFETY: the ring's bytes are only touched by the one writer (the free part)
// and the one reader (the filled part), which the counts keep apart; every
// other field is atomic.
unsafe impl Send for Pipe {}
// SAFETY: as for `Send`.
unsafe impl Sync for Pipe {}

impl Pipe {
    fn new(capacity: usize) -> Arc<Self> {
        let capacity = capacity.max(1);
        Arc::new(Self {
            ring: (0..capacity)
                .map(|_| core::cell::UnsafeCell::new(0))
                .collect(),
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            reader: AtomicWaker::new(),
            writer: AtomicWaker::new(),
            write_closed: AtomicBool::new(false),
            read_closed: AtomicBool::new(false),
        })
    }

    fn base(&self) -> *mut u8 {
        core::cell::UnsafeCell::raw_get(self.ring.as_ptr())
    }

    /// The reader's poll: copies out what is there, the end once the writer
    /// closed and the ring is empty, or waits.
    fn poll_read(&self, cx: &mut Context<'_>, out: &mut [u8]) -> Poll<io::Result<usize>> {
        if out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let read = self.read.load(Ordering::Relaxed);
        let mut written = self.written.load(Ordering::Acquire);
        if written == read {
            if self.write_closed.load(Ordering::Acquire) {
                return Poll::Ready(Ok(0));
            }
            self.reader.register(cx.waker());
            written = self.written.load(Ordering::SeqCst);
            if written == read {
                // The writer may have closed after the first look.
                if self.write_closed.load(Ordering::SeqCst) {
                    return Poll::Ready(Ok(0));
                }
                return Poll::Pending;
            }
        }
        let n = (written - read).min(out.len());
        let cap = self.ring.len();
        let at = read % cap;
        let first = n.min(cap - at);
        // SAFETY: bytes `read..read + n` were written and published by the
        // writer's release store, which the acquire load above saw; the
        // writer does not touch them until `read` moves past them.
        unsafe {
            core::ptr::copy_nonoverlapping(self.base().add(at), out.as_mut_ptr(), first);
            core::ptr::copy_nonoverlapping(self.base(), out.as_mut_ptr().add(first), n - first);
        }
        self.read.store(read + n, Ordering::Release);
        self.writer.wake();
        Poll::Ready(Ok(n))
    }

    /// The writer's poll: copies in what fits, fails once the reader is
    /// gone, or waits for room.
    fn poll_write(&self, cx: &mut Context<'_>, input: &[u8]) -> Poll<io::Result<usize>> {
        if self.read_closed.load(Ordering::Acquire) || self.write_closed.load(Ordering::Relaxed) {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if input.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let cap = self.ring.len();
        let written = self.written.load(Ordering::Relaxed);
        let mut read = self.read.load(Ordering::Acquire);
        if written - read == cap {
            self.writer.register(cx.waker());
            read = self.read.load(Ordering::SeqCst);
            if written - read == cap {
                if self.read_closed.load(Ordering::SeqCst) {
                    return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
                }
                return Poll::Pending;
            }
        }
        let n = (cap - (written - read)).min(input.len());
        let at = written % cap;
        let first = n.min(cap - at);
        // SAFETY: bytes `written..written + n` are free: the reader moved
        // `read` past them (seen by the acquire load) and does not touch
        // them until `written` moves past them.
        unsafe {
            core::ptr::copy_nonoverlapping(input.as_ptr(), self.base().add(at), first);
            core::ptr::copy_nonoverlapping(input.as_ptr().add(first), self.base(), n - first);
        }
        self.written.store(written + n, Ordering::Release);
        self.reader.wake();
        Poll::Ready(Ok(n))
    }

    /// The writing end closes: the reader sees the end after what is left.
    fn close_write(&self) {
        self.write_closed.store(true, Ordering::SeqCst);
        self.reader.wake();
    }

    /// The reading end goes: the writer's next write fails.
    fn close_read(&self) {
        self.read_closed.store(true, Ordering::SeqCst);
        self.writer.wake();
    }
}

/// One end of a [`duplex`] pair: what it writes the other reads, and what
/// the other writes it reads. Dropping it ends the other's reads (after what
/// is left) and fails the other's writes.
pub struct DuplexStream {
    read: Arc<Pipe>,
    write: Arc<Pipe>,
}

/// Two connected in-memory streams, each direction a pipe of
/// `max_buf_size` bytes: a write waits while its pipe is full.
///
/// ```
/// use nuclei::io::{AsyncReadExt, AsyncWriteExt};
///
/// # nuclei::block_on(async {
/// let (mut client, mut server) = nuclei::io::duplex(64);
/// client.write_all(b"ping").await.unwrap();
/// let mut buf = [0u8; 4];
/// server.read_exact(&mut buf).await.unwrap();
/// assert_eq!(&buf, b"ping");
/// # });
/// ```
pub fn duplex(max_buf_size: usize) -> (DuplexStream, DuplexStream) {
    let (ab, ba) = (Pipe::new(max_buf_size), Pipe::new(max_buf_size));
    (
        DuplexStream {
            read: Arc::clone(&ba),
            write: Arc::clone(&ab),
        },
        DuplexStream {
            read: ab,
            write: ba,
        },
    )
}

impl AsyncRead for DuplexStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        crate::coop::poll_budget(cx, |cx| self.read.poll_read(cx, buf))
    }
}

impl AsyncWrite for DuplexStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        crate::coop::poll_budget(cx, |cx| self.write.poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.write.close_write();
        Poll::Ready(Ok(()))
    }
}

impl Drop for DuplexStream {
    fn drop(&mut self) {
        self.write.close_write();
        self.read.close_read();
    }
}

impl core::fmt::Debug for DuplexStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DuplexStream").finish_non_exhaustive()
    }
}

/// The reading end of a [`simplex`] pipe. Dropping it fails the writer's
/// next write.
pub struct SimplexReader {
    pipe: Arc<Pipe>,
}

/// The writing end of a [`simplex`] pipe. Dropping it (or closing it) ends
/// the reader's reads after what is left.
pub struct SimplexWriter {
    pipe: Arc<Pipe>,
}

/// One in-memory pipe of `max_buf_size` bytes: what the writer writes, the
/// reader reads.
pub fn simplex(max_buf_size: usize) -> (SimplexReader, SimplexWriter) {
    let pipe = Pipe::new(max_buf_size);
    (
        SimplexReader {
            pipe: Arc::clone(&pipe),
        },
        SimplexWriter { pipe },
    )
}

impl AsyncRead for SimplexReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        crate::coop::poll_budget(cx, |cx| self.pipe.poll_read(cx, buf))
    }
}

impl AsyncWrite for SimplexWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        crate::coop::poll_budget(cx, |cx| self.pipe.poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.pipe.close_write();
        Poll::Ready(Ok(()))
    }
}

impl Drop for SimplexReader {
    fn drop(&mut self) {
        self.pipe.close_read();
    }
}

impl Drop for SimplexWriter {
    fn drop(&mut self) {
        self.pipe.close_write();
    }
}

impl core::fmt::Debug for SimplexReader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SimplexReader").finish_non_exhaustive()
    }
}

impl core::fmt::Debug for SimplexWriter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SimplexWriter").finish_non_exhaustive()
    }
}
