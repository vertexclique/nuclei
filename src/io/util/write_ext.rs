//! [`AsyncWriteExt`]: the writes every [`AsyncWrite`] gets, as futures.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io::{self, IoSlice};

use futures_io::AsyncWrite;

/// One typed-write method per line: name, type, conversion, what it writes.
macro_rules! write_ints {
    ($($name:ident, $ty:ty, $conv:ident, $what:literal;)+) => {$(
        #[doc = concat!("Writes ", $what, ", all of its bytes.")]
        fn $name(&mut self, n: $ty) -> WriteNumber<'_, Self, { core::mem::size_of::<$ty>() }>
        where
            Self: Unpin,
        {
            WriteNumber {
                writer: self,
                bytes: n.$conv(),
                done: 0,
            }
        }
    )+};
}

/// Writes on any [`AsyncWrite`]: a slice, all of a slice, typed numbers in
/// big or little endian; and its flush and shutdown.
pub trait AsyncWriteExt: AsyncWrite {
    /// Writes some of `buf`, answering how many bytes.
    fn write<'a>(&'a mut self, buf: &'a [u8]) -> Write<'a, Self>
    where
        Self: Unpin,
    {
        Write { writer: self, buf }
    }

    /// Writes some of the slices `bufs`, in order, answering how many bytes.
    fn write_vectored<'a, 'b>(&'a mut self, bufs: &'a [IoSlice<'b>]) -> WriteVectored<'a, 'b, Self>
    where
        Self: Unpin,
    {
        WriteVectored { writer: self, bufs }
    }

    /// Writes all of `buf`; a writer that takes no more bytes is a
    /// [`WriteZero`](io::ErrorKind::WriteZero) error.
    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> WriteAll<'a, Self>
    where
        Self: Unpin,
    {
        WriteAll { writer: self, buf }
    }

    /// Writes out what the writer holds back, down to its destination.
    fn flush(&mut self) -> Flush<'_, Self>
    where
        Self: Unpin,
    {
        Flush { writer: self }
    }

    /// Flushes and closes the writing side (`poll_close`): a socket sends
    /// its end of stream, a pipe's reader sees its end.
    fn shutdown(&mut self) -> Shutdown<'_, Self>
    where
        Self: Unpin,
    {
        Shutdown { writer: self }
    }

    write_ints! {
        write_u8, u8, to_be_bytes, "an unsigned byte";
        write_i8, i8, to_be_bytes, "a signed byte";
        write_u16, u16, to_be_bytes, "a big-endian `u16`";
        write_u16_le, u16, to_le_bytes, "a little-endian `u16`";
        write_i16, i16, to_be_bytes, "a big-endian `i16`";
        write_i16_le, i16, to_le_bytes, "a little-endian `i16`";
        write_u32, u32, to_be_bytes, "a big-endian `u32`";
        write_u32_le, u32, to_le_bytes, "a little-endian `u32`";
        write_i32, i32, to_be_bytes, "a big-endian `i32`";
        write_i32_le, i32, to_le_bytes, "a little-endian `i32`";
        write_u64, u64, to_be_bytes, "a big-endian `u64`";
        write_u64_le, u64, to_le_bytes, "a little-endian `u64`";
        write_i64, i64, to_be_bytes, "a big-endian `i64`";
        write_i64_le, i64, to_le_bytes, "a little-endian `i64`";
        write_u128, u128, to_be_bytes, "a big-endian `u128`";
        write_u128_le, u128, to_le_bytes, "a little-endian `u128`";
        write_i128, i128, to_be_bytes, "a big-endian `i128`";
        write_i128_le, i128, to_le_bytes, "a little-endian `i128`";
        write_f32, f32, to_be_bytes, "a big-endian `f32`";
        write_f32_le, f32, to_le_bytes, "a little-endian `f32`";
        write_f64, f64, to_be_bytes, "a big-endian `f64`";
        write_f64_le, f64, to_le_bytes, "a little-endian `f64`";
    }
}

impl<W: AsyncWrite + ?Sized> AsyncWriteExt for W {}

/// The future of [`write`](AsyncWriteExt::write).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Write<'a, W: ?Sized> {
    writer: &'a mut W,
    buf: &'a [u8],
}

impl<W: AsyncWrite + Unpin + ?Sized> Future for Write<'_, W> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut *this.writer).poll_write(cx, this.buf)
    }
}

/// The future of [`write_vectored`](AsyncWriteExt::write_vectored).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct WriteVectored<'a, 'b, W: ?Sized> {
    writer: &'a mut W,
    bufs: &'a [IoSlice<'b>],
}

impl<W: AsyncWrite + Unpin + ?Sized> Future for WriteVectored<'_, '_, W> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut *this.writer).poll_write_vectored(cx, this.bufs)
    }
}

/// The future of [`write_all`](AsyncWriteExt::write_all).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct WriteAll<'a, W: ?Sized> {
    writer: &'a mut W,
    buf: &'a [u8],
}

impl<W: AsyncWrite + Unpin + ?Sized> Future for WriteAll<'_, W> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        while !this.buf.is_empty() {
            let n = ready!(Pin::new(&mut *this.writer).poll_write(cx, this.buf))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            this.buf = &this.buf[n..];
        }
        Poll::Ready(Ok(()))
    }
}

/// The future of a typed write ([`write_u32`](AsyncWriteExt::write_u32) and
/// the like).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct WriteNumber<'a, W: ?Sized, const N: usize> {
    writer: &'a mut W,
    bytes: [u8; N],
    done: usize,
}

impl<W: AsyncWrite + Unpin + ?Sized, const N: usize> Future for WriteNumber<'_, W, N> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        while this.done < N {
            let n = ready!(Pin::new(&mut *this.writer).poll_write(cx, &this.bytes[this.done..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            this.done += n;
        }
        Poll::Ready(Ok(()))
    }
}

/// The future of [`flush`](AsyncWriteExt::flush).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Flush<'a, W: ?Sized> {
    writer: &'a mut W,
}

impl<W: AsyncWrite + Unpin + ?Sized> Future for Flush<'_, W> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().writer).poll_flush(cx)
    }
}

/// The future of [`shutdown`](AsyncWriteExt::shutdown).
#[derive(Debug)]
#[must_use = "futures do nothing unless polled"]
pub struct Shutdown<'a, W: ?Sized> {
    writer: &'a mut W,
}

impl<W: AsyncWrite + Unpin + ?Sized> Future for Shutdown<'_, W> {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().writer).poll_close(cx)
    }
}
