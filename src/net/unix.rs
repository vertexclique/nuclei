//! Unix domain sockets: listeners, streams and datagrams.

use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::{SocketAddr as UnixSocketAddr, UnixDatagram, UnixListener, UnixStream};
use std::path::Path;

use futures_core::Stream;

use crate::io::source::write::{Dest, Peer};
use crate::io::source::{Addr, Kind};
use crate::io::{Handle, sys};

fn unix(addr: Addr) -> io::Result<UnixSocketAddr> {
    match addr {
        Addr::Unix(addr) => Ok(addr),
        Addr::Inet(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a Unix socket's peer has an internet address",
        )),
    }
}

impl Handle<UnixListener> {
    /// A listener bound to `path`.
    ///
    /// # Errors
    ///
    /// As [`UnixListener::bind`].
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Handle<UnixListener>> {
        Handle::adopt(UnixListener::bind(path)?, Kind::UnixListener)
    }

    /// The next connection, with its peer's address.
    ///
    /// # Errors
    ///
    /// As `accept(2)`.
    pub async fn accept(&self) -> io::Result<(Handle<UnixStream>, UnixSocketAddr)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    fn poll_accept(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(Handle<UnixStream>, UnixSocketAddr)>> {
        self.source().poll_accept(cx).map(|accepted| {
            let (fd, addr) = accepted?;
            Ok((stream(fd), unix(addr)?))
        })
    }

    /// The connections as they come, without their addresses; the stream
    /// never ends, and an error is one of its items.
    pub fn incoming(
        &self,
    ) -> impl Stream<Item = io::Result<Handle<UnixStream>>> + Send + Unpin + '_ {
        Incoming { listener: self }
    }
}

fn stream(fd: OwnedFd) -> Handle<UnixStream> {
    Handle::from_nonblocking(UnixStream::from(fd), Kind::Stream)
}

struct Incoming<'a> {
    listener: &'a Handle<UnixListener>,
}

impl Stream for Incoming<'_> {
    type Item = io::Result<Handle<UnixStream>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.listener
            .poll_accept(cx)
            .map(|accepted| Some(accepted.map(|(stream, _)| stream)))
    }
}

impl Handle<UnixStream> {
    /// A stream connected to the listener at `path`.
    ///
    /// # Errors
    ///
    /// As `connect(2)`.
    pub async fn connect<P: AsRef<Path>>(path: P) -> io::Result<Handle<UnixStream>> {
        let stream = Handle::from_nonblocking(
            UnixStream::from(sys::socket(libc::AF_UNIX, libc::SOCK_STREAM)?),
            Kind::Stream,
        );
        let peer = Peer::Unix(path.as_ref());
        match poll_fn(|cx| stream.source().poll_connect(cx, peer)).await {
            Ok(()) => Ok(stream),
            // The listener's backlog is full: nothing tells a socket when it
            // has room, so a thread of the blocking pool waits in a blocking
            // connect instead.
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                drop(stream);
                let path = path.as_ref().to_path_buf();
                let connected =
                    crate::runtime::spawn_blocking(move || UnixStream::connect(path)).await?;
                Handle::adopt(connected, Kind::Stream)
            }
            Err(err) => Err(err),
        }
    }

    /// Two streams connected to each other.
    ///
    /// # Errors
    ///
    /// As `socketpair(2)`.
    pub fn pair() -> io::Result<(Handle<UnixStream>, Handle<UnixStream>)> {
        let (a, b) = UnixStream::pair()?;
        Ok((
            Handle::adopt(a, Kind::Stream)?,
            Handle::adopt(b, Kind::Stream)?,
        ))
    }

    /// Sends bytes; returns how many were taken.
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

impl Handle<UnixDatagram> {
    /// A socket bound to `path`.
    ///
    /// # Errors
    ///
    /// As [`UnixDatagram::bind`].
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Handle<UnixDatagram>> {
        Handle::adopt(UnixDatagram::bind(path)?, Kind::UnixDatagram)
    }

    /// Two sockets connected to each other.
    ///
    /// # Errors
    ///
    /// As `socketpair(2)`.
    pub fn pair() -> io::Result<(Handle<UnixDatagram>, Handle<UnixDatagram>)> {
        let (a, b) = UnixDatagram::pair()?;
        Ok((
            Handle::adopt(a, Kind::UnixDatagram)?,
            Handle::adopt(b, Kind::UnixDatagram)?,
        ))
    }

    /// Sends a datagram to the connected peer.
    ///
    /// # Errors
    ///
    /// As `send(2)`, or an earlier datagram's.
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        poll_fn(|cx| self.source().poll_send(cx, buf, Dest::Connected)).await
    }

    /// Receives a datagram from the connected peer; what does not fit `buf`
    /// is cut.
    ///
    /// # Errors
    ///
    /// As `recv(2)`.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.receive(buf, false).await.map(|(n, _)| n)
    }

    /// Looks at the next datagram without taking it.
    ///
    /// # Errors
    ///
    /// As `recv(2)` with `MSG_PEEK`.
    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.receive(buf, true).await.map(|(n, _)| n)
    }

    /// Sends a datagram to the socket at `path`.
    ///
    /// # Errors
    ///
    /// As `sendto(2)`, or an earlier datagram's.
    pub async fn send_to<P: AsRef<Path>>(&self, buf: &[u8], path: P) -> io::Result<usize> {
        let path = path.as_ref();
        poll_fn(|cx| self.source().poll_send(cx, buf, Dest::Unix(path))).await
    }

    /// Receives a datagram with its sender's address.
    ///
    /// # Errors
    ///
    /// As `recvfrom(2)`.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        self.receive(buf, false).await
    }

    /// Looks at the next datagram and its sender without taking it.
    ///
    /// # Errors
    ///
    /// As `recvfrom(2)` with `MSG_PEEK`.
    pub async fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        self.receive(buf, true).await
    }

    async fn receive(&self, buf: &mut [u8], peek: bool) -> io::Result<(usize, UnixSocketAddr)> {
        let (n, addr) = poll_fn(|cx| self.source().poll_receive(cx, buf, peek)).await?;
        Ok((n, unix(addr)?))
    }
}
