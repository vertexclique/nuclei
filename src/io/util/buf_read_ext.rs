//! [`AsyncBufReadExt`] and [`AsyncSeekExt`]: the reads every buffered reader
//! gets (up to a byte, a line, line by line) and the seeks every seekable
//! stream gets, as futures.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io::{self, SeekFrom};

use futures_io::{AsyncBufRead, AsyncSeek};

/// Reads on any [`AsyncBufRead`]: up to a delimiter, a line, the lines or
/// the segments one by one.
pub trait AsyncBufReadExt: AsyncBufRead {
    /// Reads up to and including `byte` (or to the end), appending to `buf`;
    /// answers how many bytes it appended (0 at the end).
    fn read_until<'a>(&'a mut self, byte: u8, buf: &'a mut Vec<u8>) -> ReadUntil<'a, Self>
    where
        Self: Unpin,
    {
        ReadUntil {
            reader: self,
            byte,
            buf,
            read: 0,
        }
    }

    /// Reads up to and including a newline (or to the end), appending to
    /// `buf` if what it read is UTF-8 (an
    /// [`InvalidData`](io::ErrorKind::InvalidData) error and `buf` as it was
    /// otherwise); answers how many bytes it appended (0 at the end).
    fn read_line<'a>(&'a mut self, buf: &'a mut String) -> ReadLine<'a, Self>
    where
        Self: Unpin,
    {
        ReadLine {
            reader: self,
            buf,
            bytes: Vec::new(),
            read: 0,
        }
    }

    /// The lines, without their `\n` or `\r\n`, one by one through
    /// [`Lines::next_line`].
    fn lines(self) -> Lines<Self>
    where
        Self: Sized,
    {
        Lines {
            reader: self,
            bytes: Vec::new(),
            read: 0,
        }
    }

    /// The segments between `byte`s, without it, one by one through
    /// [`Split::next_segment`].
    fn split(self, byte: u8) -> Split<Self>
    where
        Self: Sized,
    {
        Split {
            reader: self,
            byte,
            buf: Vec::new(),
            read: 0,
        }
    }

    /// The reader's buffer, filled if it was empty (empty at the end).
    fn fill_buf(&mut self) -> FillBuf<'_, Self>
    where
        Self: Unpin,
    {
        FillBuf { reader: Some(self) }
    }

    /// Marks `amount` bytes of the buffer read.
    fn consume(&mut self, amount: usize)
    where
        Self: Unpin,
    {
        Pin::new(self).consume(amount);
    }
}

impl<R: AsyncBufRead + ?Sized> AsyncBufReadExt for R {}

/// Reads up to and including `byte`, appending to `buf` and counting into
/// `read`; ready at the byte or the end.
fn read_until<R: AsyncBufRead + ?Sized>(
    mut reader: Pin<&mut R>,
    cx: &mut Context<'_>,
    byte: u8,
    buf: &mut Vec<u8>,
    read: &mut usize,
) -> Poll<io::Result<usize>> {
    loop {
        let (done, used) = {
            let available = ready!(reader.as_mut().poll_fill_buf(cx))?;
            match memchr(byte, available) {
                Some(i) => {
                    buf.extend_from_slice(&available[..=i]);
                    (true, i + 1)
                }
                None => {
                    buf.extend_from_slice(available);
                    (available.is_empty(), available.len())
                }
            }
        };
        reader.as_mut().consume(used);
        *read += used;
        if done {
            return Poll::Ready(Ok(core::mem::take(read)));
        }
    }
}

fn memchr(byte: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == byte)
}

/// The future of [`read_until`](AsyncBufReadExt::read_until).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct ReadUntil<'a, R: ?Sized> {
    reader: &'a mut R,
    byte: u8,
    buf: &'a mut Vec<u8>,
    read: usize,
}

impl<R: AsyncBufRead + Unpin + ?Sized> Future for ReadUntil<'_, R> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        read_until(
            Pin::new(&mut *this.reader),
            cx,
            this.byte,
            this.buf,
            &mut this.read,
        )
    }
}

/// The future of [`read_line`](AsyncBufReadExt::read_line).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct ReadLine<'a, R: ?Sized> {
    reader: &'a mut R,
    buf: &'a mut String,
    bytes: Vec<u8>,
    read: usize,
}

impl<R: AsyncBufRead + Unpin + ?Sized> Future for ReadLine<'_, R> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let n = ready!(read_until(
            Pin::new(&mut *this.reader),
            cx,
            b'\n',
            &mut this.bytes,
            &mut this.read
        ))?;
        let bytes = core::mem::take(&mut this.bytes);
        match String::from_utf8(bytes) {
            Ok(line) => {
                this.buf.push_str(&line);
                Poll::Ready(Ok(n))
            }
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the line is not UTF-8",
            ))),
        }
    }
}

/// The lines of a buffered reader; made by
/// [`lines`](AsyncBufReadExt::lines).
#[derive(Debug)]
pub struct Lines<R> {
    reader: R,
    bytes: Vec<u8>,
    read: usize,
}

impl<R: AsyncBufRead + Unpin> Lines<R> {
    /// The next line without its `\n` or `\r\n`, `None` at the end.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        core::future::poll_fn(|cx| self.poll_next_line(cx)).await
    }

    /// Polls for the next line.
    pub fn poll_next_line(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<String>>> {
        let n = ready!(read_until(
            Pin::new(&mut self.reader),
            cx,
            b'\n',
            &mut self.bytes,
            &mut self.read
        ))?;
        if n == 0 && self.bytes.is_empty() {
            return Poll::Ready(Ok(None));
        }
        let mut bytes = core::mem::take(&mut self.bytes);
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        match String::from_utf8(bytes) {
            Ok(line) => Poll::Ready(Ok(Some(line))),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the line is not UTF-8",
            ))),
        }
    }

    /// The reader, taken back.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

/// The segments of a buffered reader between a byte; made by
/// [`split`](AsyncBufReadExt::split).
#[derive(Debug)]
pub struct Split<R> {
    reader: R,
    byte: u8,
    buf: Vec<u8>,
    read: usize,
}

impl<R: AsyncBufRead + Unpin> Split<R> {
    /// The next segment without its delimiter, `None` at the end.
    pub async fn next_segment(&mut self) -> io::Result<Option<Vec<u8>>> {
        core::future::poll_fn(|cx| self.poll_next_segment(cx)).await
    }

    /// Polls for the next segment.
    pub fn poll_next_segment(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<Vec<u8>>>> {
        let n = ready!(read_until(
            Pin::new(&mut self.reader),
            cx,
            self.byte,
            &mut self.buf,
            &mut self.read
        ))?;
        if n == 0 && self.buf.is_empty() {
            return Poll::Ready(Ok(None));
        }
        let mut segment = core::mem::take(&mut self.buf);
        if segment.last() == Some(&self.byte) {
            segment.pop();
        }
        Poll::Ready(Ok(Some(segment)))
    }

    /// The reader, taken back.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

/// The future of [`fill_buf`](AsyncBufReadExt::fill_buf).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct FillBuf<'a, R: ?Sized> {
    reader: Option<&'a mut R>,
}

impl<'a, R: AsyncBufRead + Unpin + ?Sized> Future for FillBuf<'a, R> {
    type Output = io::Result<&'a [u8]>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&'a [u8]>> {
        let this = self.get_mut();
        let Some(reader) = this.reader.take() else {
            panic!("`FillBuf` polled after it completed");
        };
        match Pin::new(&mut *reader).poll_fill_buf(cx) {
            Poll::Ready(Ok(slice)) => {
                // SAFETY: `reader` is the `&'a mut R` taken out of the future,
                // which never uses it again, so the buffer it lends may live
                // for `'a`. Only the borrow checker's handling of a borrow
                // returned on one arm and dropped on the other needs this.
                let slice = unsafe { core::mem::transmute::<&[u8], &'a [u8]>(slice) };
                Poll::Ready(Ok(slice))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => {
                this.reader = Some(reader);
                Poll::Pending
            }
        }
    }
}

/// Seeks on any [`AsyncSeek`].
pub trait AsyncSeekExt: AsyncSeek {
    /// Moves the stream's position, answering the new one from the start.
    fn seek(&mut self, pos: SeekFrom) -> Seek<'_, Self>
    where
        Self: Unpin,
    {
        Seek { stream: self, pos }
    }

    /// Moves the stream's position to the start.
    fn rewind(&mut self) -> Seek<'_, Self>
    where
        Self: Unpin,
    {
        self.seek(SeekFrom::Start(0))
    }

    /// The stream's position from the start.
    fn stream_position(&mut self) -> Seek<'_, Self>
    where
        Self: Unpin,
    {
        self.seek(SeekFrom::Current(0))
    }
}

impl<S: AsyncSeek + ?Sized> AsyncSeekExt for S {}

/// The future of [`seek`](AsyncSeekExt::seek) and the seeks built on it.
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Seek<'a, S: ?Sized> {
    stream: &'a mut S,
    pos: SeekFrom,
}

impl<S: AsyncSeek + Unpin + ?Sized> Future for Seek<'_, S> {
    type Output = io::Result<u64>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        Pin::new(&mut *this.stream).poll_seek(cx, this.pos)
    }
}
