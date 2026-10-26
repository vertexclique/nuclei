//! The process's standard streams as async streams: [`stdin`], [`stdout`] and
//! [`stderr`].
//!
//! A standard stream may be a terminal, a pipe or a file the shell redirected,
//! and no one poller waits on all three; so their reads and writes run on the
//! blocking pool, one at a time per handle. A read takes what one blocking
//! read gave and hands the rest out on later reads. A write is copied (up to
//! [`MAX_BUF`] bytes at once) and answered at once while the pool writes it;
//! the next write, a flush or a close waits for it, and reports its error if
//! it failed.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io;

use futures_io::{AsyncRead, AsyncWrite};

use crate::task::Join;

/// The most bytes one blocking read or write of a standard stream moves.
const MAX_BUF: usize = 16 * 1024;

/// What a blocking operation hands back: its result, the buffer, the stream.
type Done<T> = (io::Result<usize>, Vec<u8>, T);

/// A blocking stream driven through the blocking pool.
#[derive(Debug)]
enum Blocking<T> {
    /// Ready for the next operation; `buf[pos..]` holds bytes read and not
    /// yet handed out.
    Idle { inner: T, buf: Vec<u8>, pos: usize },
    /// An operation on the pool.
    Busy(Join<Done<T>>),
    /// The stream was lost: its operation's task did not come back.
    Gone,
}

impl<T: Send + 'static> Blocking<T> {
    fn new(inner: T) -> Self {
        Self::Idle {
            inner,
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// Waits for the operation on the pool, if one runs; answers its result
    /// (`Ok(0)` when none ran).
    fn poll_idle(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        let Self::Busy(job) = self else {
            return Poll::Ready(Ok(0));
        };
        let done = ready!(Pin::new(job).poll(cx));
        match done {
            Ok((result, buf, inner)) => {
                *self = Self::Idle { inner, buf, pos: 0 };
                Poll::Ready(result)
            }
            Err(err) => {
                *self = Self::Gone;
                Poll::Ready(Err(err.into()))
            }
        }
    }

    fn start(
        &mut self,
        op: impl FnOnce(&mut T, &mut Vec<u8>) -> io::Result<usize> + Send + 'static,
    ) {
        if let Self::Idle { inner, buf, .. } = core::mem::replace(self, Self::Gone) {
            let (mut inner, mut buf) = (inner, buf);
            *self = Self::Busy(
                crate::runtime::spawn_blocking(move || {
                    let result = op(&mut inner, &mut buf);
                    (result, buf, inner)
                })
                .join(),
            );
        }
    }

    fn gone() -> io::Error {
        io::Error::other("the standard stream was lost with its blocking task")
    }
}

impl<T: io::Read + Send + 'static> Blocking<T> {
    fn poll_read(&mut self, cx: &mut Context<'_>, out: &mut [u8]) -> Poll<io::Result<usize>> {
        loop {
            match self {
                Self::Idle { buf, pos, .. } if *pos < buf.len() => {
                    let n = (buf.len() - *pos).min(out.len());
                    out[..n].copy_from_slice(&buf[*pos..*pos + n]);
                    *pos += n;
                    return Poll::Ready(Ok(n));
                }
                Self::Idle { .. } => {
                    if out.is_empty() {
                        return Poll::Ready(Ok(0));
                    }
                    let want = out.len().min(MAX_BUF);
                    self.start(move |inner, buf| {
                        buf.clear();
                        buf.resize(want, 0);
                        let n = inner.read(buf)?;
                        buf.truncate(n);
                        Ok(n)
                    });
                }
                Self::Busy(_) => {
                    let result = ready!(self.poll_idle(cx));
                    if let Err(err) = result {
                        return Poll::Ready(Err(err));
                    }
                    if let Self::Idle { buf, .. } = self
                        && buf.is_empty()
                    {
                        return Poll::Ready(Ok(0));
                    }
                }
                Self::Gone => return Poll::Ready(Err(Self::gone())),
            }
        }
    }
}

impl<T: io::Write + Send + 'static> Blocking<T> {
    fn poll_write(&mut self, cx: &mut Context<'_>, input: &[u8]) -> Poll<io::Result<usize>> {
        ready!(self.poll_idle(cx))?;
        // A write op always carries bytes: an empty buffer after an op is how
        // `poll_flush` knows its own flush ran.
        if input.is_empty() {
            return Poll::Ready(Ok(0));
        }
        match self {
            Self::Idle { buf, .. } => {
                let n = input.len().min(MAX_BUF);
                buf.clear();
                buf.extend_from_slice(&input[..n]);
                self.start(|inner, buf| inner.write_all(buf).map(|()| buf.len()));
                Poll::Ready(Ok(n))
            }
            Self::Busy(_) => Poll::Pending,
            Self::Gone => Poll::Ready(Err(Self::gone())),
        }
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            match self {
                Self::Busy(_) => {
                    ready!(self.poll_idle(cx))?;
                    if let Self::Idle { buf, .. } = self
                        && buf.is_empty()
                    {
                        // The flush below ran: done.
                        return Poll::Ready(Ok(()));
                    }
                    // A write ran: flush after it.
                    self.start(|inner, buf| {
                        buf.clear();
                        inner.flush().map(|()| 0)
                    });
                }
                Self::Idle { .. } => {
                    self.start(|inner, buf| {
                        buf.clear();
                        inner.flush().map(|()| 0)
                    });
                }
                Self::Gone => return Poll::Ready(Err(Self::gone())),
            }
        }
    }
}

macro_rules! std_stream {
    ($(#[$doc:meta])* $name:ident, $make:ident, $std:ident, $what:literal) => {
        $(#[$doc])*
        #[derive(Debug)]
        pub struct $name {
            inner: Blocking<std::io::$std>,
        }

        #[doc = concat!("The process's ", $what, ", as an async stream (see [`", stringify!($name), "`]).")]
        pub fn $make() -> $name {
            $name {
                inner: Blocking::new(std::io::$make()),
            }
        }
    };
}

std_stream!(
    /// The process's standard input: reads run on the blocking pool.
    Stdin, stdin, Stdin, "standard input"
);
std_stream!(
    /// The process's standard output: writes run on the blocking pool, one
    /// at a time; flush before the process exits to see the last one done.
    Stdout, stdout, Stdout, "standard output"
);
std_stream!(
    /// The process's standard error: writes run on the blocking pool, one at
    /// a time.
    Stderr, stderr, Stderr, "standard error"
);

impl AsyncRead for Stdin {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().inner.poll_read(cx, buf)
    }
}

macro_rules! write_stream {
    ($($name:ident),*) => {$(
        impl AsyncWrite for $name {
            fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
                self.get_mut().inner.poll_write(cx, buf)
            }

            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.get_mut().inner.poll_flush(cx)
            }

            fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.get_mut().inner.poll_flush(cx)
            }
        }
    )*};
}
write_stream!(Stdout, Stderr);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{AsyncReadExt, AsyncWriteExt};
    use std::sync::{Arc, Mutex};

    /// A blocking writer that records what reaches it and its flushes.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<(Vec<u8>, usize)>>);

    impl io::Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().0.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.lock().unwrap().1 += 1;
            Ok(())
        }
    }

    struct Wrap<T>(Blocking<T>);

    impl<T: io::Write + Send + Unpin + 'static> AsyncWrite for Wrap<T> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().0.poll_write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut().0.poll_flush(cx)
        }
        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut().0.poll_flush(cx)
        }
    }

    impl<T: io::Read + Send + Unpin + 'static> AsyncRead for Wrap<T> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().0.poll_read(cx, buf)
        }
    }

    #[test]
    fn writes_reach_the_stream_in_order_and_a_flush_waits_for_them() {
        crate::block_on(async {
            let record = Recorder::default();
            let mut w = Wrap(Blocking::new(record.clone()));
            let big = vec![b'b'; MAX_BUF * 2 + 5];
            w.write_all(b"").await.unwrap();
            w.flush().await.unwrap();
            assert_eq!(
                record.0.lock().unwrap().1,
                1,
                "a flush after an empty write flushes"
            );
            w.write_all(b"head ").await.unwrap();
            w.write_all(&big).await.unwrap();
            w.write_all(b" tail").await.unwrap();
            w.flush().await.unwrap();
            let (bytes, flushes) = record.0.lock().unwrap().clone();
            assert_eq!(bytes.len(), 5 + big.len() + 5);
            assert!(bytes.starts_with(b"head ") && bytes.ends_with(b" tail"));
            assert_eq!(flushes, 2);
        });
    }

    #[test]
    fn reads_hand_out_what_one_blocking_read_gave() {
        crate::block_on(async {
            let mut r = Wrap(Blocking::new(&b"abcdefghij"[..]));
            let mut three = [0u8; 3];
            r.read_exact(&mut three).await.unwrap();
            assert_eq!(&three, b"abc");
            let mut rest = Vec::new();
            r.read_to_end(&mut rest).await.unwrap();
            assert_eq!(rest, b"defghij");
        });
    }

    /// A blocking writer that fails.
    struct Broken;

    impl io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_write_is_reported_by_the_next_operation() {
        crate::block_on(async {
            let mut w = Wrap(Blocking::new(Broken));
            assert_eq!(
                w.write(b"x").await.unwrap(),
                1,
                "answered while the pool writes it"
            );
            let err = w.flush().await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        });
    }

    #[test]
    fn the_process_streams_take_writes() {
        crate::block_on(async {
            let mut out = stdout();
            out.write_all(b"").await.unwrap();
            out.flush().await.unwrap();
            let mut err = stderr();
            err.flush().await.unwrap();
        });
    }
}
