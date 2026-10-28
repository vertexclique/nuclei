//! Buffering around a reader or a writer: [`BufReader`], [`BufWriter`], and
//! both at once, [`BufStream`].

use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io::{self, IoSlice, SeekFrom};

use futures_io::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite};

/// The buffer the wrappers make when no capacity is given.
const DEFAULT_CAPACITY: usize = 8 * 1024;

/// A reader with a buffer in front: small reads take from the buffer, which
/// is filled by large reads of the reader. Writes, if the reader is also a
/// writer, go straight through.
#[derive(Debug)]
pub struct BufReader<R> {
    inner: R,
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
}

impl<R> BufReader<R> {
    /// A reader with a buffer of 8 KiB in front of `inner`.
    pub fn new(inner: R) -> Self {
        Self::with_capacity(DEFAULT_CAPACITY, inner)
    }

    /// A reader with a buffer of `capacity` bytes in front of `inner`.
    pub fn with_capacity(capacity: usize, inner: R) -> Self {
        Self {
            inner,
            buf: vec![0; capacity].into_boxed_slice(),
            pos: 0,
            cap: 0,
        }
    }

    /// The reader inside.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// The reader inside, to change (reading it directly skips the bytes
    /// buffered).
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// The reader inside, pinned.
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut R>
    where
        R: Unpin,
    {
        Pin::new(&mut self.get_mut().inner)
    }

    /// The reader inside, taken back; the bytes buffered are lost.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// The bytes buffered and not read yet.
    pub fn buffer(&self) -> &[u8] {
        &self.buf[self.pos..self.cap]
    }

    fn discard_buffer(&mut self) {
        self.pos = 0;
        self.cap = 0;
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for BufReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        // An empty buffer and a read at least its size: the buffer would only
        // add a copy.
        if this.pos == this.cap && out.len() >= this.buf.len() {
            let n = ready!(Pin::new(&mut this.inner).poll_read(cx, out))?;
            this.discard_buffer();
            return Poll::Ready(Ok(n));
        }
        let available = ready!(Pin::new(&mut *this).poll_fill_buf(cx))?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        Pin::new(this).consume(n);
        Poll::Ready(Ok(n))
    }
}

impl<R: AsyncRead + Unpin> AsyncBufRead for BufReader<R> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if this.pos >= this.cap {
            let n = ready!(Pin::new(&mut this.inner).poll_read(cx, &mut this.buf))?;
            this.cap = n;
            this.pos = 0;
        }
        Poll::Ready(Ok(&this.buf[this.pos..this.cap]))
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        let this = self.get_mut();
        this.pos = (this.pos + amount).min(this.cap);
    }
}

impl<R: AsyncRead + AsyncSeek + Unpin> AsyncSeek for BufReader<R> {
    /// Seeks the reader; a seek relative to the current position counts
    /// from the position of the next byte read through the buffer. The
    /// buffer is emptied.
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        let pos = match pos {
            SeekFrom::Current(n) => {
                let buffered = (this.cap - this.pos) as i64;
                match n.checked_sub(buffered) {
                    Some(n) => SeekFrom::Current(n),
                    None => {
                        // Past `i64`'s range: two seeks, the buffered part first.
                        ready!(
                            Pin::new(&mut this.inner).poll_seek(cx, SeekFrom::Current(-buffered))
                        )?;
                        this.discard_buffer();
                        SeekFrom::Current(n)
                    }
                }
            }
            other => other,
        };
        let at = ready!(Pin::new(&mut this.inner).poll_seek(cx, pos))?;
        this.discard_buffer();
        Poll::Ready(Ok(at))
    }
}

impl<R: AsyncWrite + Unpin> AsyncWrite for BufReader<R> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

/// A writer with a buffer in front: small writes gather in the buffer, which
/// goes to the writer when full, on a flush, or on a close. Reads, if the
/// writer is also a reader, go straight through. Dropping it loses what is
/// still buffered: flush or close it first.
#[derive(Debug)]
pub struct BufWriter<W> {
    inner: W,
    buf: Vec<u8>,
    written: usize,
}

impl<W> BufWriter<W> {
    /// A writer with a buffer of 8 KiB in front of `inner`.
    pub fn new(inner: W) -> Self {
        Self::with_capacity(DEFAULT_CAPACITY, inner)
    }

    /// A writer with a buffer of `capacity` bytes in front of `inner`.
    pub fn with_capacity(capacity: usize, inner: W) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(capacity),
            written: 0,
        }
    }

    /// The writer inside.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// The writer inside, to change (writing it directly puts the bytes
    /// ahead of what is buffered).
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// The writer inside, pinned.
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut W>
    where
        W: Unpin,
    {
        Pin::new(&mut self.get_mut().inner)
    }

    /// The writer inside, taken back; the bytes buffered are lost.
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// The bytes buffered and not written to the writer yet.
    pub fn buffer(&self) -> &[u8] {
        &self.buf[self.written..]
    }
}

impl<W: AsyncWrite + Unpin> BufWriter<W> {
    /// Writes the buffer out to the writer, all of it.
    fn flush_buf(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.written < self.buf.len() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.buf[self.written..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "the writer took no more of the buffered bytes",
                )));
            }
            self.written += n;
        }
        self.buf.clear();
        self.written = 0;
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for BufWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.buf.len() + buf.len() > this.buf.capacity() {
            ready!(this.flush_buf(cx))?;
        }
        if buf.len() >= this.buf.capacity() {
            // As large as the buffer: straight to the writer.
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        this.buf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let total: usize = bufs.iter().map(|b| b.len()).sum();
        if this.buf.len() + total > this.buf.capacity() {
            ready!(this.flush_buf(cx))?;
        }
        if total >= this.buf.capacity() {
            return Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        }
        for b in bufs {
            this.buf.extend_from_slice(b);
        }
        Poll::Ready(Ok(total))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.flush_buf(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.flush_buf(cx))?;
        Pin::new(&mut this.inner).poll_close(cx)
    }
}

impl<W: AsyncRead + Unpin> AsyncRead for BufWriter<W> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<W: AsyncBufRead + Unpin> AsyncBufRead for BufWriter<W> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        Pin::new(&mut self.get_mut().inner).poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        Pin::new(&mut self.get_mut().inner).consume(amount);
    }
}

impl<W: AsyncWrite + AsyncSeek + Unpin> AsyncSeek for BufWriter<W> {
    /// Writes the buffer out, then seeks the writer.
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        ready!(this.flush_buf(cx))?;
        Pin::new(&mut this.inner).poll_seek(cx, pos)
    }
}

/// A stream with a read buffer and a write buffer in front: a
/// [`BufReader`] around a [`BufWriter`].
#[derive(Debug)]
pub struct BufStream<S> {
    inner: BufReader<BufWriter<S>>,
}

impl<S> BufStream<S> {
    /// Buffers of 8 KiB each way in front of `stream`.
    pub fn new(stream: S) -> Self {
        Self {
            inner: BufReader::new(BufWriter::new(stream)),
        }
    }

    /// A read buffer of `read` bytes and a write buffer of `write` bytes in
    /// front of `stream`.
    pub fn with_capacity(read: usize, write: usize, stream: S) -> Self {
        Self {
            inner: BufReader::with_capacity(read, BufWriter::with_capacity(write, stream)),
        }
    }

    /// The stream inside.
    pub fn get_ref(&self) -> &S {
        self.inner.get_ref().get_ref()
    }

    /// The stream inside, to change.
    pub fn get_mut(&mut self) -> &mut S {
        self.inner.get_mut().get_mut()
    }

    /// The stream inside, taken back; the bytes buffered either way are lost.
    pub fn into_inner(self) -> S {
        self.inner.into_inner().into_inner()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for BufStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncBufRead for BufStream<S> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        Pin::new(&mut self.get_mut().inner).poll_fill_buf(cx)
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        Pin::new(&mut self.get_mut().inner).consume(amount);
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for BufStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}
