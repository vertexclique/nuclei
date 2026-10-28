//! Copying from a reader to a writer: [`copy`], [`copy_buf`] and, both ways
//! at once in one task, [`copy_bidirectional`].

use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io;

use futures_io::{AsyncBufRead, AsyncRead, AsyncWrite};

/// The buffer [`copy`] and [`copy_bidirectional`] use for each direction.
const DEFAULT_BUF_SIZE: usize = 8 * 1024;

/// One direction of a copy: a buffer, what is in it, and the count copied.
#[derive(Debug)]
struct CopyBuffer {
    read_done: bool,
    need_flush: bool,
    pos: usize,
    cap: usize,
    amount: u64,
    buf: Box<[u8]>,
}

impl CopyBuffer {
    fn new(size: usize) -> Self {
        Self {
            read_done: false,
            need_flush: false,
            pos: 0,
            cap: 0,
            amount: 0,
            buf: vec![0; size.max(1)].into_boxed_slice(),
        }
    }

    /// Copies until the reader ends and the writer is flushed; answers the
    /// bytes copied. Before it waits on the reader it flushes what it wrote,
    /// so nothing waits in the writer meanwhile.
    fn poll_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<u64>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        loop {
            if self.pos == self.cap && !self.read_done {
                match reader.as_mut().poll_read(cx, &mut self.buf) {
                    Poll::Ready(Ok(0)) => self.read_done = true,
                    Poll::Ready(Ok(n)) => {
                        self.pos = 0;
                        self.cap = n;
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => {
                        if self.need_flush {
                            ready!(writer.as_mut().poll_flush(cx))?;
                            self.need_flush = false;
                        }
                        return Poll::Pending;
                    }
                }
            }
            while self.pos < self.cap {
                let n = ready!(
                    writer
                        .as_mut()
                        .poll_write(cx, &self.buf[self.pos..self.cap])
                )?;
                if n == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "the writer took no more bytes",
                    )));
                }
                self.pos += n;
                self.amount += n as u64;
                self.need_flush = true;
            }
            if self.pos == self.cap && self.read_done {
                ready!(writer.as_mut().poll_flush(cx))?;
                return Poll::Ready(Ok(self.amount));
            }
        }
    }
}

/// Copies everything `reader` gives into `writer`, then flushes it; answers
/// the bytes copied.
///
/// ```
/// # nuclei::block_on(async {
/// let mut reader: &[u8] = b"hello";
/// let mut writer = Vec::new();
/// let copied = nuclei::io::copy(&mut reader, &mut writer).await.unwrap();
/// assert_eq!(copied, 5);
/// assert_eq!(writer, b"hello");
/// # });
/// ```
pub async fn copy<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut buf = CopyBuffer::new(DEFAULT_BUF_SIZE);
    poll_fn(|cx| buf.poll_copy(cx, Pin::new(&mut *reader), Pin::new(&mut *writer))).await
}

/// Copies everything a buffered `reader` gives into `writer` from the
/// reader's own buffer, then flushes it; answers the bytes copied.
pub async fn copy_buf<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
where
    R: AsyncBufRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut amount = 0u64;
    poll_fn(|cx| {
        loop {
            let mut reader = Pin::new(&mut *reader);
            let writer = Pin::new(&mut *writer);
            let available = ready!(reader.as_mut().poll_fill_buf(cx))?;
            if available.is_empty() {
                ready!(writer.poll_flush(cx))?;
                return Poll::Ready(Ok(amount));
            }
            let n = ready!(writer.poll_write(cx, available))?;
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the writer took no more bytes",
                )));
            }
            reader.consume(n);
            amount += n as u64;
        }
    })
    .await
}

/// One direction of [`copy_bidirectional`].
#[derive(Debug)]
enum Transfer {
    Running(CopyBuffer),
    ShuttingDown(u64),
    Done(u64),
}

impl Transfer {
    /// Copies, then closes the writer once the reader ended: the peer of the
    /// writer sees the end the reader saw.
    fn poll<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<u64>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        if let Self::Running(buf) = self {
            let amount = ready!(buf.poll_copy(cx, reader, writer.as_mut()))?;
            *self = Self::ShuttingDown(amount);
        }
        self.poll_after_copy(cx, writer)
    }

    fn poll_after_copy<W: AsyncWrite + ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        writer: Pin<&mut W>,
    ) -> Poll<io::Result<u64>> {
        match *self {
            Self::ShuttingDown(amount) => {
                ready!(writer.poll_close(cx))?;
                *self = Self::Done(amount);
                Poll::Ready(Ok(amount))
            }
            Self::Done(amount) => Poll::Ready(Ok(amount)),
            Self::Running(_) => Poll::Pending,
        }
    }
}

/// Copies `a` into `b` and `b` into `a` at once, in the calling task, each
/// direction through a buffer of 8 KiB; a direction whose reader ends closes
/// its writer. Answers the bytes copied each way, `(a to b, b to a)`, once
/// both ended; the first error ends both.
pub async fn copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    copy_bidirectional_with_sizes(a, b, DEFAULT_BUF_SIZE, DEFAULT_BUF_SIZE).await
}

/// [`copy_bidirectional`] with a buffer of `a_to_b` bytes for one direction
/// and of `b_to_a` bytes for the other.
pub async fn copy_bidirectional_with_sizes<A, B>(
    a: &mut A,
    b: &mut B,
    a_to_b: usize,
    b_to_a: usize,
) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let mut forward = Transfer::Running(CopyBuffer::new(a_to_b));
    let mut backward = Transfer::Running(CopyBuffer::new(b_to_a));
    poll_fn(|cx| {
        let ab = forward.poll(cx, Pin::new(&mut *a), Pin::new(&mut *b))?;
        let ba = backward.poll(cx, Pin::new(&mut *b), Pin::new(&mut *a))?;
        match (ab, ba) {
            (Poll::Ready(ab), Poll::Ready(ba)) => Poll::Ready(Ok((ab, ba))),
            _ => Poll::Pending,
        }
    })
    .await
}
