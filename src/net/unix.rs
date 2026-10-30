//! Unix domain sockets: listeners, streams and datagrams, and [`UCred`], the
//! credentials of a stream's peer; and Unix pipes ([`pipe`]).

pub mod pipe;
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
use crate::io::sys::UnixAddr;
use crate::io::{Handle, Interest, IoObject, Ready, sys};

/// The credentials of a Unix stream's peer: its user and group, and its
/// process where the system tells it (Linux, Android, macOS, iOS).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UCred {
    uid: u32,
    gid: u32,
    pid: Option<i32>,
}

impl UCred {
    /// The peer's user id.
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// The peer's group id.
    pub fn gid(&self) -> u32 {
        self.gid
    }

    /// The peer's process id, where the system tells it.
    pub fn pid(&self) -> Option<i32> {
        self.pid
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_cred(fd: std::os::fd::RawFd) -> io::Result<UCred> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED answers a `ucred`, written into our own.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            core::ptr::from_mut(&mut cred).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(UCred {
        uid: cred.uid,
        gid: cred.gid,
        pid: Some(cred.pid),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_cred(fd: std::os::fd::RawFd) -> io::Result<UCred> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: the call writes the two ids into our own.
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let pid = {
        let mut pid: libc::pid_t = 0;
        let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: LOCAL_PEERPID answers a `pid_t`, written into our own.
        let r = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                core::ptr::from_mut(&mut pid).cast(),
                &mut len,
            )
        };
        (r == 0).then_some(pid)
    };
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let pid = None;
    Ok(UCred { uid, gid, pid })
}

fn unix(addr: Addr) -> io::Result<UnixSocketAddr> {
    match addr {
        Addr::Unix(addr) => Ok(addr.into_inner()),
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

    /// A listener bound to `addr`: a path, or on Linux a name in the
    /// abstract namespace.
    ///
    /// # Errors
    ///
    /// As [`UnixListener::bind_addr`].
    pub fn bind_addr(addr: &UnixSocketAddr) -> io::Result<Handle<UnixListener>> {
        Handle::adopt(UnixListener::bind_addr(addr)?, Kind::UnixListener)
    }

    /// The next connection, with its peer's address. On macOS an accept
    /// that finds the process out of descriptors closes the connection it
    /// took, and the next ones take none until a descriptor is free, as a
    /// TCP listener's do.
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

/// The connect of the non-blocking Unix stream `socket` to `addr`.
pub(super) async fn connect(socket: OwnedFd, addr: &UnixAddr) -> io::Result<Handle<UnixStream>> {
    let stream = Handle::from_nonblocking(UnixStream::from(socket), Kind::Stream);
    match poll_fn(|cx| stream.source().poll_connect(cx, Peer::Unix(addr))).await {
        Ok(()) => Ok(stream),
        // The listener's backlog is full: nothing tells a socket when it
        // has room, so a thread of the blocking pool waits in a blocking
        // connect instead.
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            drop(stream);
            let addr = *addr;
            let connected =
                crate::rt::blocking::call(move || sys::connect_unix_waiting(&addr)).await?;
            Ok(Handle::from_nonblocking(
                UnixStream::from(connected),
                Kind::Stream,
            ))
        }
        Err(err) => Err(err),
    }
}

impl Handle<UnixStream> {
    /// A stream connected to the listener at `path`.
    ///
    /// # Errors
    ///
    /// As `connect(2)`.
    pub async fn connect<P: AsRef<Path>>(path: P) -> io::Result<Handle<UnixStream>> {
        connect(
            sys::socket(libc::AF_UNIX, libc::SOCK_STREAM)?,
            &UnixAddr::path(path.as_ref())?,
        )
        .await
    }

    /// A stream connected to the listener at `addr`: a path, or on Linux a
    /// name in the abstract namespace.
    ///
    /// # Errors
    ///
    /// As `connect(2)`; an unnamed address names no listener.
    pub async fn connect_addr(addr: &UnixSocketAddr) -> io::Result<Handle<UnixStream>> {
        connect(
            sys::socket(libc::AF_UNIX, libc::SOCK_STREAM)?,
            &UnixAddr::of(addr)?,
        )
        .await
    }

    /// The credentials of the process at the other end, as the connection
    /// was made.
    ///
    /// # Errors
    ///
    /// When the system cannot tell them.
    pub fn peer_cred(&self) -> io::Result<UCred> {
        peer_cred(self.raw_io())
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

    /// A socket bound to `addr`: a path, or on Linux a name in the abstract
    /// namespace.
    ///
    /// # Errors
    ///
    /// As [`UnixDatagram::bind_addr`].
    pub fn bind_addr(addr: &UnixSocketAddr) -> io::Result<Handle<UnixDatagram>> {
        Handle::adopt(UnixDatagram::bind_addr(addr)?, Kind::UnixDatagram)
    }

    /// A socket bound to no address: it sends with `send_to`, or to the peer
    /// its `connect` names.
    ///
    /// # Errors
    ///
    /// When the system has no socket to give.
    pub fn unbound() -> io::Result<Handle<UnixDatagram>> {
        Ok(Handle::from_nonblocking(
            UnixDatagram::from(sys::socket(libc::AF_UNIX, libc::SOCK_DGRAM)?),
            Kind::UnixDatagram,
        ))
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
    /// As `send(2)` of this datagram.
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

    /// Sends all of `buf` as one datagram to the connected peer and gives
    /// it back with its length. The driver sends from `buf` itself, so the
    /// bytes are never copied.
    ///
    /// If the future is dropped before it completes, the datagram is sent
    /// whole or not at all: one the driver has not handed to the kernel is
    /// withdrawn, and one already handed over goes out.
    pub async fn send_owned(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.source().send_owned(buf, Dest::Connected).await
    }

    /// Sends all of `buf` as one datagram to the socket at `path` and gives
    /// it back with its length, as [`send_owned`](Self::send_owned) sends to
    /// the connected peer.
    pub async fn send_to_owned<P: AsRef<Path>>(
        &self,
        buf: Vec<u8>,
        path: P,
    ) -> (io::Result<usize>, Vec<u8>) {
        match UnixAddr::path(path.as_ref()) {
            Ok(addr) => self.source().send_owned(buf, Dest::Unix(&addr)).await,
            Err(err) => (Err(err), buf),
        }
    }

    /// Receives a datagram from the connected peer into the spare capacity
    /// of `buf`, after the bytes it holds, and gives `buf` back with the
    /// datagram's length. The driver receives into `buf` itself, so the bytes
    /// are never copied; what does not fit is cut.
    ///
    /// If the future is dropped before it completes, a datagram the driver
    /// received into `buf` stays with the socket for its next receive, so
    /// none is lost.
    pub async fn recv_owned(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        let (result, buf) = self.recv_from_owned(buf).await;
        (result.map(|(n, _)| n), buf)
    }

    /// Receives a datagram with its sender's address into the spare capacity
    /// of `buf`, as [`recv_owned`](Self::recv_owned).
    pub async fn recv_from_owned(
        &self,
        buf: Vec<u8>,
    ) -> (io::Result<(usize, UnixSocketAddr)>, Vec<u8>) {
        let (result, buf) = self.source().recv_from_owned(buf).await;
        (
            result.and_then(|(n, addr)| unix(addr).map(|addr| (n, addr))),
            buf,
        )
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
    /// As `sendto(2)` of this datagram.
    pub async fn send_to<P: AsRef<Path>>(&self, buf: &[u8], path: P) -> io::Result<usize> {
        let addr = UnixAddr::path(path.as_ref())?;
        poll_fn(|cx| self.source().poll_send(cx, buf, Dest::Unix(&addr))).await
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
        poll_fn(|cx| self.poll_receive(cx, buf, peek)).await
    }

    fn poll_receive(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<(usize, UnixSocketAddr)>> {
        let (n, addr) = core::task::ready!(self.source().poll_receive(cx, buf, peek))?;
        Poll::Ready(unix(addr).map(|addr| (n, addr)))
    }

    /// Waits until a receive would end at once: a datagram came, or an
    /// error (which the receive reports).
    ///
    /// # Errors
    ///
    /// When the socket cannot be watched.
    pub async fn readable(&self) -> io::Result<()> {
        self.ready(Interest::READABLE).await.map(drop)
    }

    /// Waits until a send would be taken at once.
    ///
    /// # Errors
    ///
    /// When the socket cannot be watched.
    pub async fn writable(&self) -> io::Result<()> {
        self.ready(Interest::WRITABLE).await.map(drop)
    }

    /// Waits until some of `interest` is ready, and says what is.
    ///
    /// # Errors
    ///
    /// When the socket cannot be watched.
    pub async fn ready(&self, interest: Interest) -> io::Result<Ready> {
        poll_fn(|cx| super::poll_ready(self.source(), cx, interest)).await
    }

    /// Polls until a receive would end at once.
    ///
    /// # Errors
    ///
    /// When the socket cannot be watched.
    pub fn poll_recv_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.source().poll_read_ready(cx).map(|r| r.map(drop))
    }

    /// Polls until a send would be taken at once.
    ///
    /// # Errors
    ///
    /// When the socket cannot be watched.
    pub fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.source().poll_write_ready(cx).map(|r| r.map(drop))
    }

    /// Sends a datagram to the connected peer without waiting.
    ///
    /// # Errors
    ///
    /// `WouldBlock` when a datagram is still queued, and as `send(2)`.
    pub fn try_send(&self, buf: &[u8]) -> io::Result<usize> {
        super::at_once(|cx| self.source().poll_send(cx, buf, Dest::Connected))
    }

    /// Sends a datagram to the socket at `path` without waiting.
    ///
    /// # Errors
    ///
    /// `WouldBlock` when a datagram is still queued, and as `sendto(2)`.
    pub fn try_send_to<P: AsRef<Path>>(&self, buf: &[u8], path: P) -> io::Result<usize> {
        let addr = UnixAddr::path(path.as_ref())?;
        super::at_once(|cx| self.source().poll_send(cx, buf, Dest::Unix(&addr)))
    }

    /// Receives a datagram from the connected peer without waiting; what
    /// does not fit `buf` is cut.
    ///
    /// # Errors
    ///
    /// `WouldBlock` when none came yet, and as `recv(2)`.
    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        super::at_once(|cx| self.poll_receive(cx, buf, false)).map(|(n, _)| n)
    }

    /// Receives a datagram and its sender's address without waiting.
    ///
    /// # Errors
    ///
    /// `WouldBlock` when none came yet, and as `recvfrom(2)`.
    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        super::at_once(|cx| self.poll_receive(cx, buf, false))
    }
}
