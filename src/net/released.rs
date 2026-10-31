//! The buffer of a zero-copy write, handed back once the kernel let go of it.

use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::buf::owned::{IoBuf, OwnedBuf};

/// The buffer of a [`TcpStream::write_zero_copy`](crate::net::TcpStream::write_zero_copy), as a
/// future that resolves to it once the kernel let go of every byte. For a send the kernel did not
/// copy, that is once the peer acknowledged the bytes, which may be long after the write ended.
///
/// It may be polled from any thread, or dropped: the buffer is then dropped once the kernel let go
/// of it, never before. A write the kernel copied gives one that is ready at once.
///
/// The buffer comes back even after the runtime that sent it shut down: a keeper on the blocking
/// pool takes over the completions of its worker's ring then, and lets go of the ring once the
/// kernel let go of every buffer.
pub struct Released<B = Vec<u8>> {
    inner: Inner,
    buf: PhantomData<fn() -> B>,
}

enum Inner {
    Ready(Option<OwnedBuf>),
    #[cfg(ring)]
    Waiting(crate::sync::oneshot::Receiver<OwnedBuf>),
}

impl<B> Released<B> {
    /// One that is ready with `buf`.
    pub(crate) fn ready(buf: OwnedBuf) -> Self {
        Self {
            inner: Inner::Ready(Some(buf)),
            buf: PhantomData,
        }
    }

    /// One whose buffer comes through `back` once its last hold let go.
    #[cfg(ring)]
    pub(crate) fn waiting(back: crate::sync::oneshot::Receiver<OwnedBuf>) -> Self {
        Self {
            inner: Inner::Waiting(back),
            buf: PhantomData,
        }
    }
}

impl<B> Future for Released<B>
where
    B: IoBuf,
{
    type Output = B;

    /// The buffer once the kernel let go of it. Polled again after that, an empty buffer.
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<B> {
        #[cfg(not(ring))]
        let _ = cx;
        let back = match &mut self.inner {
            Inner::Ready(buf) => buf.take().unwrap_or_default(),
            // The last hold always sends before it goes: an error is only a receiver polled
            // again after it resolved.
            #[cfg(ring)]
            Inner::Waiting(back) => match Pin::new(back).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(buf) => buf.unwrap_or_default(),
            },
        };
        Poll::Ready(back.into_written())
    }
}

impl<B> core::fmt::Debug for Released<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ready = match &self.inner {
            Inner::Ready(_) => true,
            #[cfg(ring)]
            Inner::Waiting(back) => !back.is_empty(),
        };
        f.debug_struct("Released").field("ready", &ready).finish()
    }
}
