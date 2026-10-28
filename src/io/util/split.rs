//! [`split`]: a stream that reads and writes, as a read half and a write half
//! two tasks can own apart.
//!
//! The halves share the stream behind a flag each poll call holds for its
//! length (a poll of the stream, never an await), so a contended call spins
//! for at most one poll of the other half. A nuclei [`Handle`](crate::Handle)
//! needs none of this: `&Handle` reads and writes on its own.

use core::cell::UnsafeCell;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io::{self, IoSlice};
use std::sync::Arc;

use futures_io::{AsyncRead, AsyncWrite};

use crate::sync::internal::{AtomicBool, Ordering};

struct Inner<T> {
    locked: AtomicBool,
    stream: UnsafeCell<T>,
}

// SAFETY: the stream is only reached through `with`, which holds the flag:
// one half at a time, so `T: Send` is all sharing it needs.
unsafe impl<T: Send> Send for Inner<T> {}
// SAFETY: as for `Send`.
unsafe impl<T: Send> Sync for Inner<T> {}

impl<T> Inner<T> {
    /// Runs `f` on the stream, holding the flag for its length.
    fn with<R>(&self, f: impl FnOnce(Pin<&mut T>) -> R) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _release = Release(&self.locked);
        // SAFETY: the flag is held: no other reference to the stream exists;
        // the stream never moves out of the `Arc` while both halves live, so
        // pinning it is sound.
        f(unsafe { Pin::new_unchecked(&mut *self.stream.get()) })
    }
}

/// The read half of a stream [`split`] in two.
pub struct ReadHalf<T> {
    inner: Arc<Inner<T>>,
}

/// The write half of a stream [`split`] in two.
pub struct WriteHalf<T> {
    inner: Arc<Inner<T>>,
}

/// Splits `stream` into a read half and a write half; [`ReadHalf::unsplit`]
/// puts them back together.
pub fn split<T: AsyncRead + AsyncWrite>(stream: T) -> (ReadHalf<T>, WriteHalf<T>) {
    let inner = Arc::new(Inner {
        locked: AtomicBool::new(false),
        stream: UnsafeCell::new(stream),
    });
    (
        ReadHalf {
            inner: Arc::clone(&inner),
        },
        WriteHalf { inner },
    )
}

impl<T> ReadHalf<T> {
    /// Whether `other` is this half's write half.
    pub fn is_pair_of(&self, other: &WriteHalf<T>) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// The stream, back from its two halves.
    ///
    /// # Panics
    ///
    /// When `write` is not this half's write half.
    #[expect(
        clippy::unreachable,
        reason = "the other half is dropped above, so this one holds the last reference"
    )]
    pub fn unsplit(self, write: WriteHalf<T>) -> T
    where
        T: Unpin,
    {
        assert!(
            self.is_pair_of(&write),
            "unsplit of halves of two different streams"
        );
        drop(write);
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => inner.stream.into_inner(),
            Err(_) => unreachable!("both halves were here, and are gone"),
        }
    }
}

impl<T> WriteHalf<T> {
    /// Whether `other` is this half's read half.
    pub fn is_pair_of(&self, other: &ReadHalf<T>) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl<T: AsyncRead> AsyncRead for ReadHalf<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.with(|stream| stream.poll_read(cx, buf))
    }
}

impl<T: AsyncWrite> AsyncWrite for WriteHalf<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.with(|stream| stream.poll_write(cx, buf))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.inner
            .with(|stream| stream.poll_write_vectored(cx, bufs))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.with(|stream| stream.poll_flush(cx))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.with(|stream| stream.poll_close(cx))
    }
}

impl<T> core::fmt::Debug for ReadHalf<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadHalf").finish_non_exhaustive()
    }
}

impl<T> core::fmt::Debug for WriteHalf<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WriteHalf").finish_non_exhaustive()
    }
}
