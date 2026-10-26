//! UDP sockets.

use core::future::poll_fn;
use core::task::{Context, Poll};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

use super::addr::{ToSocketAddrs, resolve};
use super::none_worked;
use crate::io::source::write::Dest;
use crate::io::source::{Addr, Kind};
use crate::io::{Handle, ReadBuf};

impl Handle<UdpSocket> {
    /// A socket bound to the first of the addresses `addr` names that it
    /// can bind.
    ///
    /// # Errors
    ///
    /// The last address's error when none could be bound, or the lookup's.
    pub async fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Handle<UdpSocket>> {
        let mut last = None;
        for addr in resolve(&addr).await? {
            match UdpSocket::bind(addr) {
                Ok(socket) => return Handle::adopt(socket, Kind::Udp),
                Err(err) => last = Some(err),
            }
        }
        Err(none_worked(last))
    }

    /// A socket on an ephemeral local port, connected to the first of
    /// `sock_addrs` it can connect to: `send` and `recv` then talk to that
    /// peer only.
    ///
    /// # Errors
    ///
    /// The last address's error when none worked, or the lookup's.
    pub async fn connect<A: ToSocketAddrs>(sock_addrs: A) -> io::Result<Handle<UdpSocket>> {
        let mut last = None;
        for addr in resolve(&sock_addrs).await? {
            let local: IpAddr = if addr.is_ipv4() {
                Ipv4Addr::UNSPECIFIED.into()
            } else {
                Ipv6Addr::UNSPECIFIED.into()
            };
            let attempt = UdpSocket::bind((local, 0)).and_then(|socket| {
                socket.connect(addr)?;
                Handle::adopt(socket, Kind::Udp)
            });
            match attempt {
                Ok(socket) => return Ok(socket),
                Err(err) => last = Some(err),
            }
        }
        Err(none_worked(last))
    }

    /// Sends a datagram to the connected peer.
    ///
    /// # Errors
    ///
    /// As `send(2)`, or the error of an earlier datagram sent after its call
    /// returned.
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

    /// Sends a datagram to the first address `addr` names.
    ///
    /// # Errors
    ///
    /// As `sendto(2)`, the lookup's, or an earlier datagram's.
    pub async fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> io::Result<usize> {
        let addr = resolve(&addr)
            .await?
            .next()
            .ok_or_else(|| none_worked(None))?;
        poll_fn(|cx| self.source().poll_send(cx, buf, Dest::Inet(addr))).await
    }

    /// Receives a datagram with its sender's address.
    ///
    /// # Errors
    ///
    /// As `recvfrom(2)`.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.receive(buf, false).await
    }

    /// Looks at the next datagram and its sender without taking it.
    ///
    /// # Errors
    ///
    /// As `recvfrom(2)` with `MSG_PEEK`.
    pub async fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.receive(buf, true).await
    }

    /// The sender of the next datagram, which stays to be received.
    ///
    /// # Errors
    ///
    /// As `recvfrom(2)` with `MSG_PEEK`.
    pub async fn peek_sender(&self) -> io::Result<SocketAddr> {
        poll_fn(|cx| self.poll_peek_sender(cx)).await
    }

    /// Polls for the sender of the next datagram, which stays to be
    /// received. No byte of it is copied.
    ///
    /// # Errors
    ///
    /// As `recvfrom(2)` with `MSG_PEEK`.
    pub fn poll_peek_sender(&self, cx: &mut Context<'_>) -> Poll<io::Result<SocketAddr>> {
        self.poll_receive(cx, &mut [], true)
            .map(|r| r.map(|(_, addr)| addr))
    }

    /// Receives a datagram from the connected peer into the unfilled part of
    /// `buf`, which advances by its length; what does not fit is cut.
    ///
    /// # Errors
    ///
    /// As `recv(2)`.
    pub async fn recv_buf(&self, buf: &mut ReadBuf<'_>) -> io::Result<usize> {
        self.recv_buf_from(buf).await.map(|(n, _)| n)
    }

    /// Receives a datagram and its sender's address into the unfilled part
    /// of `buf`, which advances by its length; what does not fit is cut.
    ///
    /// # Errors
    ///
    /// As `recvfrom(2)`.
    pub async fn recv_buf_from(&self, buf: &mut ReadBuf<'_>) -> io::Result<(usize, SocketAddr)> {
        // vertexia: the unfilled part is zeroed the first time a buffer offers
        // it (a `ReadBuf` remembers what it initialized); the receive path
        // takes initialized bytes. Uninitialized memory through the receive
        // paths if a profile shows the zeroing.
        let (n, addr) = self.receive(buf.initialize_unfilled(), false).await?;
        buf.advance(n);
        Ok((n, addr))
    }

    async fn receive(&self, buf: &mut [u8], peek: bool) -> io::Result<(usize, SocketAddr)> {
        poll_fn(|cx| self.poll_receive(cx, buf, peek)).await
    }

    fn poll_receive(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
        peek: bool,
    ) -> Poll<io::Result<(usize, SocketAddr)>> {
        let (n, addr) = core::task::ready!(self.source().poll_receive(cx, buf, peek))?;
        Poll::Ready(match addr {
            Addr::Inet(addr) => Ok((n, addr)),
            #[cfg(unix)]
            Addr::Unix(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a UDP datagram came with no internet address",
            )),
        })
    }
}
