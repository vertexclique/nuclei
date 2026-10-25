//! TCP listeners and streams.

use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};

use futures_core::Stream;

use super::{addresses, none_worked};
use crate::io::source::write::Peer;
use crate::io::source::{Addr, Kind};
use crate::io::{Handle, sys};

impl Handle<TcpListener> {
    /// A listener bound to `addr`.
    ///
    /// # Errors
    ///
    /// As [`TcpListener::bind`].
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Handle<TcpListener>> {
        Handle::adopt(TcpListener::bind(addr)?, Kind::TcpListener)
    }

    /// The next connection, with its peer's address.
    ///
    /// # Errors
    ///
    /// As `accept(2)`: a connection reset before it was taken, or the
    /// process out of descriptors.
    pub async fn accept(&self) -> io::Result<(Handle<TcpStream>, SocketAddr)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(Handle<TcpStream>, SocketAddr)>> {
        self.source().poll_accept(cx).map(|accepted| {
            let (fd, addr) = accepted?;
            Ok((stream(fd), inet(addr)?))
        })
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
    /// it was taken is skipped; any other error ends the stream, and
    /// [`TcpStreamGenerator::take_error`] says what it was.
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
}

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
            match this.listener.poll_accept(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok((stream, _))) => return Poll::Ready(Some(stream)),
                Poll::Ready(Err(err)) if err.kind() == io::ErrorKind::ConnectionAborted => {}
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
        for addr in addresses(sock_addrs)? {
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
}

/// One connect: a non-blocking socket, its connect, and the driver seeing
/// it end.
async fn connect(addr: SocketAddr) -> io::Result<Handle<TcpStream>> {
    #[cfg(unix)]
    let socket = {
        let domain = if addr.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        sys::socket(domain, libc::SOCK_STREAM)?
    };
    #[cfg(windows)]
    let socket = sys::tcp_socket(if addr.is_ipv4() {
        crate::syscore::windows::AF_INET
    } else {
        crate::syscore::windows::AF_INET6
    })?;
    let stream = Handle::from_nonblocking(TcpStream::from(socket), Kind::Tcp);
    poll_fn(|cx| stream.source().poll_connect(cx, Peer::Inet(addr))).await?;
    Ok(stream)
}
