//! TCP listeners and streams.

use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};

use futures_core::Stream;

use super::addr::{ToSocketAddrs, resolve};
use super::none_worked;
use std::time::Duration;

use crate::io::IoObject;
use crate::io::raw::Owned;
use crate::io::source::write::Peer;
use crate::io::source::{Addr, Kind};
use crate::io::{Handle, sys};
use crate::rt::counters::out_of_descriptors;
use crate::time::{Sleep, sleep};

impl Handle<TcpListener> {
    /// A listener bound to the first of the addresses `addr` names that
    /// it can bind.
    ///
    /// # Errors
    ///
    /// The last address's error when none could be bound, or the lookup's.
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Handle<TcpListener>> {
        let mut last = None;
        for addr in resolve(&addr).await? {
            match TcpListener::bind(addr) {
                Ok(listener) => return Handle::adopt(listener, Kind::TcpListener),
                Err(err) => last = Some(err),
            }
        }
        Err(none_worked(last))
    }

    /// The next connection, with its peer's address.
    ///
    /// On macOS an accept that finds the process out of descriptors closes
    /// the connection it took off the backlog, as the system's own does; the
    /// next ones fail with the same error and take none until a descriptor
    /// is free.
    ///
    /// # Errors
    ///
    /// As `accept(2)`: a connection reset before it was taken, or the
    /// process out of descriptors.
    pub async fn accept(&self) -> io::Result<(Handle<TcpStream>, SocketAddr)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    /// Polls for the next connection, with its peer's address: `Pending`
    /// with `cx`'s waker kept until one comes.
    ///
    /// # Errors
    ///
    /// As [`accept`](Self::accept).
    pub fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(Handle<TcpStream>, SocketAddr)>> {
        self.source().poll_accept(cx).map(|accepted| {
            let (fd, addr) = accepted?;
            Ok((stream(fd), inet(addr)?))
        })
    }

    /// A connection, without its address: on io_uring the listener's
    /// accept stays armed for every connection that comes, taking them
    /// ahead for this handle.
    fn poll_accept_stream(&self, cx: &mut Context<'_>) -> Poll<io::Result<Handle<TcpStream>>> {
        self.source()
            .poll_accept_stream(cx)
            .map(|accepted| accepted.map(stream))
    }

    /// The connections as they come, without their addresses; the stream
    /// never ends, and an error is one of its items.
    pub fn incoming(
        &self,
    ) -> impl Stream<Item = io::Result<Handle<TcpStream>>> + Send + Unpin + '_ {
        Incoming { listener: self }
    }

    /// The connections as they come, from a stream that owns its own copy of
    /// the listener, so it may outlive this handle. A connection reset before
    /// it was taken is skipped, and so is the process having no descriptor
    /// left to accept into: the stream waits, with a growing pause, until
    /// there is one, and the connections wait in the listener's backlog (on
    /// macOS but for the one the first accept to find none took, which the
    /// system closes, as [`accept`](Self::accept) says).
    /// Any other error ends the stream, and
    /// [`TcpStreamGenerator::take_error`] says what it was.
    ///
    /// On io_uring the stream keeps one accept armed for every connection
    /// that comes, so it takes connections ahead of its polls; those it took
    /// and never yielded close when it drops, as a listener's own backlog
    /// does when the last copy of the listener closes.
    ///
    /// # Errors
    ///
    /// When the listener's descriptor cannot be copied.
    pub async fn accept_multi(&self) -> io::Result<TcpStreamGenerator> {
        let listener = self.get_ref().try_clone()?;
        Ok(TcpStreamGenerator {
            listener: Handle::adopt(listener, Kind::TcpListener)?,
            error: None,
            done: false,
            backoff: None,
            delay: OUT_OF_DESCRIPTORS_DELAY,
        })
    }
}

/// An accepted connection, set up already.
fn stream(fd: crate::io::raw::Owned) -> Handle<TcpStream> {
    Handle::from_nonblocking(TcpStream::from(fd), Kind::Tcp)
}

fn inet(addr: Addr) -> io::Result<SocketAddr> {
    match addr {
        Addr::Inet(addr) => Ok(addr),
        #[cfg(unix)]
        Addr::Unix(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a TCP listener's peer has no internet address",
        )),
    }
}

struct Incoming<'a> {
    listener: &'a Handle<TcpListener>,
}

impl Stream for Incoming<'_> {
    type Item = io::Result<Handle<TcpStream>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.listener
            .poll_accept(cx)
            .map(|accepted| Some(accepted.map(|(stream, _)| stream)))
    }
}

/// The connections of a listener, as [`Handle::accept_multi`] makes them.
#[derive(Debug)]
pub struct TcpStreamGenerator {
    listener: Handle<TcpListener>,
    error: Option<io::Error>,
    done: bool,
    backoff: Option<Sleep>,
    delay: Duration,
}

/// The first wait of a generator whose process has no descriptor to accept
/// into, doubled up to [`OUT_OF_DESCRIPTORS_DELAY_MAX`] while it lasts.
const OUT_OF_DESCRIPTORS_DELAY: Duration = Duration::from_millis(10);
const OUT_OF_DESCRIPTORS_DELAY_MAX: Duration = Duration::from_secs(1);

impl TcpStreamGenerator {
    /// Why the stream ended, once it has.
    pub fn take_error(&mut self) -> Option<io::Error> {
        self.error.take()
    }
}

impl Stream for TcpStreamGenerator {
    type Item = Handle<TcpStream>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        while !this.done {
            if let Some(backoff) = this.backoff.as_mut() {
                if Pin::new(backoff).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.backoff = None;
            }
            match this.listener.poll_accept_stream(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(stream)) => {
                    this.delay = OUT_OF_DESCRIPTORS_DELAY;
                    return Poll::Ready(Some(stream));
                }
                Poll::Ready(Err(err)) if err.kind() == io::ErrorKind::ConnectionAborted => {}
                Poll::Ready(Err(err)) if out_of_descriptors(&err) => {
                    this.backoff = Some(sleep(this.delay));
                    this.delay = (this.delay * 2).min(OUT_OF_DESCRIPTORS_DELAY_MAX);
                }
                Poll::Ready(Err(err)) => {
                    this.error = Some(err);
                    this.done = true;
                }
            }
        }
        Poll::Ready(None)
    }
}

impl Handle<TcpStream> {
    /// A stream connected to the first of `sock_addrs` that takes the
    /// connection.
    ///
    /// # Errors
    ///
    /// The last address's error when none took it, or the lookup's.
    pub async fn connect<A: ToSocketAddrs>(sock_addrs: A) -> io::Result<Handle<TcpStream>> {
        let mut last = None;
        for addr in resolve(&sock_addrs).await? {
            match connect(addr).await {
                Ok(stream) => return Ok(stream),
                Err(err) => last = Some(err),
            }
        }
        Err(none_worked(last))
    }

    /// Sends bytes; returns how many were taken (into the write-behind, as
    /// for [`AsyncWrite`]).
    ///
    /// [`AsyncWrite`]: futures_io::AsyncWrite
    ///
    /// # Errors
    ///
    /// As `send(2)`, or the error that stopped earlier bytes.
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        poll_fn(|cx| self.source().poll_write(cx, buf)).await
    }

    /// Receives bytes; zero at the end of the stream.
    ///
    /// # Errors
    ///
    /// As `recv(2)`.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| self.source().poll_read(cx, buf, false)).await
    }

    /// Looks at the bytes the next receive returns, without taking them.
    ///
    /// # Errors
    ///
    /// As `recv(2)` with `MSG_PEEK`.
    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        poll_fn(|cx| self.source().poll_read(cx, buf, true)).await
    }

    /// Polls for the bytes the next receive returns, into the unfilled part
    /// of `buf`, without taking them; `buf` advances by what came.
    ///
    /// # Errors
    ///
    /// As `recv(2)` with `MSG_PEEK`.
    pub fn poll_peek(
        &self,
        cx: &mut Context<'_>,
        buf: &mut crate::io::ReadBuf<'_>,
    ) -> Poll<io::Result<usize>> {
        let n = core::task::ready!(self.source().poll_read(cx, buf.initialize_unfilled(), true))?;
        buf.advance(n);
        Poll::Ready(Ok(n))
    }

    /// `SO_LINGER`: how long a close waits for unsent data, `None` if it
    /// does not wait.
    pub fn linger(&self) -> io::Result<Option<Duration>> {
        super::opt::linger_time(self.raw_io())
    }

    /// Sets `SO_LINGER`: with `Some(time)` a close waits up to `time` (whole
    /// seconds) for unsent data; with `None` it returns at once and the
    /// system sends what is left.
    pub fn set_linger(&self, linger: Option<Duration>) -> io::Result<()> {
        super::opt::set_linger_time(self.raw_io(), linger)
    }

    /// Sets `SO_LINGER` to zero: the close drops unsent data and resets the
    /// connection.
    pub fn set_zero_linger(&self) -> io::Result<()> {
        super::opt::set_linger_time(self.raw_io(), Some(Duration::ZERO))
    }
}

/// A new non-blocking TCP socket, IPv6 or IPv4.
pub(super) fn new_socket(v6: bool) -> io::Result<Owned> {
    #[cfg(unix)]
    return sys::socket(
        if v6 { libc::AF_INET6 } else { libc::AF_INET },
        libc::SOCK_STREAM,
    );
    #[cfg(windows)]
    return sys::tcp_socket(if v6 {
        crate::syscore::windows::AF_INET6
    } else {
        crate::syscore::windows::AF_INET
    });
}

/// One connect: a non-blocking socket, its connect, and the driver seeing
/// it end.
async fn connect(addr: SocketAddr) -> io::Result<Handle<TcpStream>> {
    connect_socket(new_socket(addr.is_ipv6())?, true, addr).await
}

/// The connect of `socket`, non-blocking already or not, to `addr`.
pub(super) async fn connect_socket(
    socket: Owned,
    nonblocking: bool,
    addr: SocketAddr,
) -> io::Result<Handle<TcpStream>> {
    let stream = if nonblocking {
        Handle::from_nonblocking(TcpStream::from(socket), Kind::Tcp)
    } else {
        Handle::adopt(TcpStream::from(socket), Kind::Tcp)?
    };
    poll_fn(|cx| stream.source().poll_connect(cx, Peer::Inet(addr))).await?;
    Ok(stream)
}
