//! The buffer of a zero-copy write, handed back once the kernel let go of it.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// The buffer of a [`TcpStream::write_zero_copy`](crate::net::TcpStream::write_zero_copy), as a
/// future that resolves to it once the kernel let go of every byte. For a send the kernel did not
/// copy, that is once the peer acknowledged the bytes, which may be long after the write ended.
///
/// It may be polled from any thread, or dropped: the buffer is then dropped once the kernel let go
/// of it, never before. A write the kernel copied gives one that is ready at once.
///
/// The buffer comes back as the runtime thread that sent it handles its completions. If that
/// thread's IO stops for good (its runtime shut down) while the kernel still holds part of the
/// buffer, the buffer is kept for good, since the kernel may still read it, and this never
/// resolves.
pub struct Released(Inner);

enum Inner {
    Ready(Option<Vec<u8>>),
    #[cfg(ring)]
    Waiting(crate::sync::oneshot::Receiver<Vec<u8>>),
}

impl Released {
    /// One that is ready with `buf`.
    pub(crate) fn ready(buf: Vec<u8>) -> Self {
        Self(Inner::Ready(Some(buf)))
    }

    /// One whose buffer comes through `back` once its last hold let go.
    #[cfg(ring)]
    pub(crate) fn waiting(back: crate::sync::oneshot::Receiver<Vec<u8>>) -> Self {
        Self(Inner::Waiting(back))
    }
}

impl Future for Released {
    type Output = Vec<u8>;

    /// The buffer once the kernel let go of it. Polled again after that, an empty buffer.
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Vec<u8>> {
        #[cfg(not(ring))]
        let _ = cx;
        match &mut self.0 {
            Inner::Ready(buf) => Poll::Ready(buf.take().unwrap_or_default()),
            // The last hold always sends before it goes: an error is only a receiver polled
            // again after it resolved.
            #[cfg(ring)]
            Inner::Waiting(back) => Pin::new(back).poll(cx).map(Result::unwrap_or_default),
        }
    }
}

impl core::fmt::Debug for Released {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ready = match &self.0 {
            Inner::Ready(_) => true,
            #[cfg(ring)]
            Inner::Waiting(back) => !back.is_empty(),
        };
        f.debug_struct("Released").field("ready", &ready).finish()
    }
}
