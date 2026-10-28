//! [`AsyncReadExt`]: the reads every [`AsyncRead`] gets, as futures.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io;

use futures_io::AsyncRead;

use super::ReadBuf;

/// One typed-read method per line: name, type, width, conversion, what it
/// reads.
macro_rules! read_ints {
    ($($name:ident, $ty:ty, $n:literal, $conv:ident, $what:literal;)+) => {$(
        #[doc = concat!("Reads ", $what, " (", stringify!($n), " bytes, exactly).")]
        fn $name(&mut self) -> ReadNumber<'_, Self, $ty, $n>
        where
            Self: Unpin,
        {
            ReadNumber {
                reader: self,
                bytes: [0; $n],
                done: 0,
                convert: <$ty>::$conv,
            }
        }
    )+};
}

/// Reads on any [`AsyncRead`]: into a slice, a [`ReadBuf`], to the end, as
/// typed numbers in big or little endian.
pub trait AsyncReadExt: AsyncRead {
    /// Reads some bytes into `buf`, answering how many (0 at the end).
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> Read<'a, Self>
    where
        Self: Unpin,
    {
        Read { reader: self, buf }
    }

    /// Reads some bytes into the unfilled part of `buf` and counts them
    /// filled, answering how many (0 at the end, or when `buf` is full).
    fn read_buf<'a, 'b>(&'a mut self, buf: &'a mut ReadBuf<'b>) -> ReadBufFuture<'a, 'b, Self>
    where
        Self: Unpin,
    {
        ReadBufFuture { reader: self, buf }
    }

    /// Fills `buf` exactly, answering its length; an end of the stream
    /// first is an [`UnexpectedEof`](io::ErrorKind::UnexpectedEof) error.
    fn read_exact<'a>(&'a mut self, buf: &'a mut [u8]) -> ReadExact<'a, Self>
    where
        Self: Unpin,
    {
        ReadExact {
            reader: self,
            buf,
            done: 0,
        }
    }

    /// Reads to the end of the stream, appending to `buf`; answers how many
    /// bytes it appended.
    fn read_to_end<'a>(&'a mut self, buf: &'a mut Vec<u8>) -> ReadToEnd<'a, Self>
    where
        Self: Unpin,
    {
        let start = buf.len();
        ReadToEnd {
            reader: self,
            buf,
            start,
            zeroed: 0,
        }
    }

    /// Reads to the end of the stream, appending to `buf` if what it read is
    /// UTF-8 (an [`InvalidData`](io::ErrorKind::InvalidData) error and `buf`
    /// as it was otherwise); answers how many bytes it appended.
    fn read_to_string<'a>(&'a mut self, buf: &'a mut String) -> ReadToString<'a, Self>
    where
        Self: Unpin,
    {
        ReadToString {
            reader: self,
            buf,
            bytes: Vec::new(),
            zeroed: 0,
        }
    }

    /// A reader that ends after `limit` bytes of this one.
    fn take(self, limit: u64) -> Take<Self>
    where
        Self: Sized,
    {
        Take { inner: self, limit }
    }

    /// A reader that reads this one to its end, then `next`.
    fn chain<R: AsyncRead>(self, next: R) -> Chain<Self, R>
    where
        Self: Sized,
    {
        Chain {
            first: self,
            second: next,
            first_done: false,
        }
    }

    read_ints! {
        read_u8, u8, 1, from_be_bytes, "an unsigned byte";
        read_i8, i8, 1, from_be_bytes, "a signed byte";
        read_u16, u16, 2, from_be_bytes, "a big-endian `u16`";
        read_u16_le, u16, 2, from_le_bytes, "a little-endian `u16`";
        read_i16, i16, 2, from_be_bytes, "a big-endian `i16`";
        read_i16_le, i16, 2, from_le_bytes, "a little-endian `i16`";
        read_u32, u32, 4, from_be_bytes, "a big-endian `u32`";
        read_u32_le, u32, 4, from_le_bytes, "a little-endian `u32`";
        read_i32, i32, 4, from_be_bytes, "a big-endian `i32`";
        read_i32_le, i32, 4, from_le_bytes, "a little-endian `i32`";
        read_u64, u64, 8, from_be_bytes, "a big-endian `u64`";
        read_u64_le, u64, 8, from_le_bytes, "a little-endian `u64`";
        read_i64, i64, 8, from_be_bytes, "a big-endian `i64`";
        read_i64_le, i64, 8, from_le_bytes, "a little-endian `i64`";
        read_u128, u128, 16, from_be_bytes, "a big-endian `u128`";
        read_u128_le, u128, 16, from_le_bytes, "a little-endian `u128`";
        read_i128, i128, 16, from_be_bytes, "a big-endian `i128`";
        read_i128_le, i128, 16, from_le_bytes, "a little-endian `i128`";
        read_f32, f32, 4, from_be_bytes, "a big-endian `f32`";
        read_f32_le, f32, 4, from_le_bytes, "a little-endian `f32`";
        read_f64, f64, 8, from_be_bytes, "a big-endian `f64`";
        read_f64_le, f64, 8, from_le_bytes, "a little-endian `f64`";
    }
}

impl<R: AsyncRead + ?Sized> AsyncReadExt for R {}

/// The future of [`read`](AsyncReadExt::read).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Read<'a, R: ?Sized> {
    reader: &'a mut R,
    buf: &'a mut [u8],
}

impl<R: AsyncRead + Unpin + ?Sized> Future for Read<'_, R> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut *this.reader).poll_read(cx, this.buf)
    }
}

/// The future of [`read_buf`](AsyncReadExt::read_buf).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct ReadBufFuture<'a, 'b, R: ?Sized> {
    reader: &'a mut R,
    buf: &'a mut ReadBuf<'b>,
}

impl<R: AsyncRead + Unpin + ?Sized> Future for ReadBufFuture<'_, '_, R> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        poll_read_buf(Pin::new(&mut *this.reader), cx, this.buf)
    }
}

/// Reads into the unfilled part of `buf` and counts what came as filled.
pub(crate) fn poll_read_buf<R: AsyncRead + ?Sized>(
    reader: Pin<&mut R>,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
) -> Poll<io::Result<usize>> {
    if buf.remaining() == 0 {
        return Poll::Ready(Ok(0));
    }
    let n = ready!(reader.poll_read(cx, buf.initialize_unfilled()))?;
    buf.advance(n);
    Poll::Ready(Ok(n))
}

/// The future of [`read_exact`](AsyncReadExt::read_exact).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct ReadExact<'a, R: ?Sized> {
    reader: &'a mut R,
    buf: &'a mut [u8],
    done: usize,
}

impl<R: AsyncRead + Unpin + ?Sized> Future for ReadExact<'_, R> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(fill(
            Pin::new(&mut *this.reader),
            cx,
            this.buf,
            &mut this.done
        ))?;
        Poll::Ready(Ok(this.buf.len()))
    }
}

/// Reads until `buf[*done..]` is full; an end first is an error.
fn fill<R: AsyncRead + ?Sized>(
    mut reader: Pin<&mut R>,
    cx: &mut Context<'_>,
    buf: &mut [u8],
    done: &mut usize,
) -> Poll<io::Result<()>> {
    while *done < buf.len() {
        let n = ready!(reader.as_mut().poll_read(cx, &mut buf[*done..]))?;
        if n == 0 {
            return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
        }
        *done += n;
    }
    Poll::Ready(Ok(()))
}

/// The future of a typed read ([`read_u32`](AsyncReadExt::read_u32) and the
/// like).
#[must_use = "futures do nothing unless polled"]
pub struct ReadNumber<'a, R: ?Sized, T, const N: usize> {
    reader: &'a mut R,
    bytes: [u8; N],
    done: usize,
    convert: fn([u8; N]) -> T,
}

impl<R: AsyncRead + Unpin + ?Sized, T, const N: usize> Future for ReadNumber<'_, R, T, N> {
    type Output = io::Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        let this = self.get_mut();
        ready!(fill(
            Pin::new(&mut *this.reader),
            cx,
            &mut this.bytes,
            &mut this.done
        ))?;
        Poll::Ready(Ok((this.convert)(this.bytes)))
    }
}

impl<R: ?Sized, T, const N: usize> core::fmt::Debug for ReadNumber<'_, R, T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadNumber")
            .field("width", &N)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

/// The future of [`read_to_end`](AsyncReadExt::read_to_end).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct ReadToEnd<'a, R: ?Sized> {
    reader: &'a mut R,
    buf: &'a mut Vec<u8>,
    start: usize,
    /// Bytes of `buf`'s spare capacity already zeroed.
    zeroed: usize,
}

impl<R: AsyncRead + Unpin + ?Sized> Future for ReadToEnd<'_, R> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(read_to_end(
            Pin::new(&mut *this.reader),
            cx,
            this.buf,
            &mut this.zeroed
        ))?;
        Poll::Ready(Ok(this.buf.len() - this.start))
    }
}

/// The least room a read to the end asks of the vector, and the most one
/// read takes: the vector grows by doubling in between.
const MIN_READ: usize = 32;
const MAX_READ: usize = 64 * 1024;

/// Reads to the end, appending to `buf`. Reads land in `buf`'s spare
/// capacity, at most [`MAX_READ`] bytes a read; when less than [`MIN_READ`]
/// is spare, the vector grows by doubling (by [`MAX_READ`] at most). Spare
/// memory is zeroed once, when it is new (`zeroed` counts what already is,
/// across polls), not before each read.
fn read_to_end<R: AsyncRead + ?Sized>(
    mut reader: Pin<&mut R>,
    cx: &mut Context<'_>,
    buf: &mut Vec<u8>,
    zeroed: &mut usize,
) -> Poll<io::Result<()>> {
    loop {
        if buf.capacity() - buf.len() < MIN_READ {
            let before = buf.capacity();
            buf.reserve(buf.len().clamp(MIN_READ, MAX_READ));
            if buf.capacity() != before {
                // A new allocation keeps only the bytes in use.
                *zeroed = 0;
            }
        }
        let spare = buf.spare_capacity_mut();
        let room = spare.len().min(MAX_READ);
        if *zeroed < room {
            for byte in &mut spare[*zeroed..room] {
                byte.write(0);
            }
            *zeroed = room;
        }
        // SAFETY: the first `room` spare bytes are initialized (zeroed now
        // or before, and only ever written with bytes since).
        let dst = unsafe { core::slice::from_raw_parts_mut(spare.as_mut_ptr().cast::<u8>(), room) };
        let n = ready!(reader.as_mut().poll_read(cx, dst))?;
        if n == 0 {
            return Poll::Ready(Ok(()));
        }
        if n > room {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the reader reported more bytes than it was given room for",
            )));
        }
        // SAFETY: the first `n <= room` spare bytes are initialized and hold
        // what the reader read.
        unsafe { buf.set_len(buf.len() + n) };
        *zeroed -= n;
    }
}

/// The future of [`read_to_string`](AsyncReadExt::read_to_string).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct ReadToString<'a, R: ?Sized> {
    reader: &'a mut R,
    buf: &'a mut String,
    bytes: Vec<u8>,
    zeroed: usize,
}

impl<R: AsyncRead + Unpin + ?Sized> Future for ReadToString<'_, R> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(read_to_end(
            Pin::new(&mut *this.reader),
            cx,
            &mut this.bytes,
            &mut this.zeroed
        ))?;
        let bytes = core::mem::take(&mut this.bytes);
        match String::from_utf8(bytes) {
            Ok(text) => {
                this.buf.push_str(&text);
                Poll::Ready(Ok(text.len()))
            }
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the stream did not hold UTF-8",
            ))),
        }
    }
}

/// A reader that ends after a number of bytes of another; made by
/// [`take`](AsyncReadExt::take).
#[derive(Debug)]
pub struct Take<R> {
    inner: R,
    limit: u64,
}

impl<R> Take<R> {
    /// The bytes left before it ends.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Sets the bytes left before it ends.
    pub fn set_limit(&mut self, limit: u64) {
        self.limit = limit;
    }

    /// The reader it reads from.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// The reader it reads from, to change.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// The reader it reads from, taken back.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Take<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.limit == 0 {
            return Poll::Ready(Ok(0));
        }
        let max = usize::try_from(this.limit).map_or(buf.len(), |l| l.min(buf.len()));
        let n = ready!(Pin::new(&mut this.inner).poll_read(cx, &mut buf[..max]))?;
        this.limit -= n as u64;
        Poll::Ready(Ok(n))
    }
}

/// A reader that reads one to its end, then another; made by
/// [`chain`](AsyncReadExt::chain).
#[derive(Debug)]
pub struct Chain<A, B> {
    first: A,
    second: B,
    first_done: bool,
}

impl<A, B> Chain<A, B> {
    /// The two readers.
    pub fn get_ref(&self) -> (&A, &B) {
        (&self.first, &self.second)
    }

    /// The two readers, to change.
    pub fn get_mut(&mut self) -> (&mut A, &mut B) {
        (&mut self.first, &mut self.second)
    }

    /// The two readers, taken back.
    pub fn into_inner(self) -> (A, B) {
        (self.first, self.second)
    }
}

impl<A: AsyncRead + Unpin, B: AsyncRead + Unpin> AsyncRead for Chain<A, B> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !this.first_done {
            let n = ready!(Pin::new(&mut this.first).poll_read(cx, buf))?;
            if n > 0 || buf.is_empty() {
                return Poll::Ready(Ok(n));
            }
            this.first_done = true;
        }
        Pin::new(&mut this.second).poll_read(cx, buf)
    }
}
