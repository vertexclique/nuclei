//! A child's piped standard streams, as async pipes: the driver reads and
//! writes them (see `io::source`), as it does any byte stream.

use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;

use futures_io::{AsyncRead, AsyncWrite};

use crate::io::Handle;

macro_rules! child_pipe {
    ($($(#[$doc:meta])* $name:ident),*) => {$(
        $(#[$doc])*
        #[derive(Debug)]
        pub struct $name(Handle<std::process::$name>);

        impl $name {
            pub(crate) fn new(pipe: std::process::$name) -> io::Result<Self> {
                Handle::new(pipe).map(Self)
            }
        }
    )*};
}

child_pipe!(
    /// The child's stdin: write to it, and drop it (or [`close`]) to end it.
    ///
    /// [`close`]: futures_io::AsyncWrite::poll_close
    ChildStdin,
    /// The child's stdout.
    ChildStdout,
    /// The child's stderr.
    ChildStderr
);

impl AsyncWrite for ChildStdin {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.source().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.source().poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.source().poll_close(cx)
    }
}

impl AsyncRead for ChildStdout {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.0.source().poll_read(cx, buf, false)
    }
}

impl AsyncRead for ChildStderr {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.0.source().poll_read(cx, buf, false)
    }
}
