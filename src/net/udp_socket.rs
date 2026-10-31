//! [`UdpSocketBuilder`]: a UDP socket made and its options set before it binds and becomes a
//! [`UdpSocket`](super::UdpSocket).
//!
//! As [`TcpSocket`](super::TcpSocket), it is its descriptor alone, each setting one system call
//! on it.

use core::mem::ManuallyDrop;
use std::io;
use std::net::SocketAddr;

use super::opt;
use super::socket::{raw_of, std_from_raw};
use crate::io::raw::{Owned, Raw};
use crate::io::source::Kind;
use crate::io::{Handle, sys};

/// A UDP socket to set up before it binds: what only a socket not bound yet takes
/// (`SO_REUSEPORT` among sockets that share a port, `IPV6_V6ONLY`, a device), and the rest
/// before the first datagram.
///
/// ```
/// use nuclei::net::UdpSocketBuilder;
///
/// # nuclei::block_on(async {
/// let builder = UdpSocketBuilder::new_v4()?;
/// builder.set_reuseaddr(true)?;
/// builder.set_recv_buffer_size(256 * 1024)?;
/// let socket = builder.bind("127.0.0.1:0".parse().unwrap())?;
/// let addr = socket.local_addr()?;
/// socket.send_to(b"hi", addr).await?;
/// let mut buf = [0u8; 2];
/// assert_eq!(socket.recv_from(&mut buf).await?, (2, addr));
/// # std::io::Result::Ok(())
/// # }).unwrap();
/// ```
pub struct UdpSocketBuilder {
    socket: Owned,
}

super::sockopt::socket_options!(UdpSocketBuilder);

impl UdpSocketBuilder {
    /// A new IPv4 UDP socket.
    ///
    /// # Errors
    ///
    /// When the system has no socket to give.
    pub fn new_v4() -> io::Result<UdpSocketBuilder> {
        Self::new(false)
    }

    /// A new IPv6 UDP socket.
    ///
    /// # Errors
    ///
    /// When the system has no socket to give.
    pub fn new_v6() -> io::Result<UdpSocketBuilder> {
        Self::new(true)
    }

    fn new(v6: bool) -> io::Result<UdpSocketBuilder> {
        #[cfg(unix)]
        let socket = sys::socket(
            if v6 { libc::AF_INET6 } else { libc::AF_INET },
            libc::SOCK_DGRAM,
        )?;
        #[cfg(windows)]
        let socket = sys::udp_socket(if v6 {
            crate::syscore::windows::AF_INET6
        } else {
            crate::syscore::windows::AF_INET
        })?;
        Ok(UdpSocketBuilder { socket })
    }

    fn raw(&self) -> Raw {
        raw_of(&self.socket)
    }

    /// The socket, seen as std's for the calls std makes.
    fn as_std(&self) -> ManuallyDrop<std::net::UdpSocket> {
        // SAFETY: the socket is ours and outlives the view, which never
        // closes it.
        ManuallyDrop::new(unsafe { std_from_raw(self.raw()) })
    }

    /// The address the socket is bound to; before it binds, the unspecified one on unix, and an
    /// error (`InvalidInput`) on Windows, as std's `local_addr` answers there.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.as_std().local_addr()
    }

    /// The socket's pending error (`SO_ERROR`), taken.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.as_std().take_error()
    }

    /// Binds the socket to `addr`, and makes it a [`UdpSocket`](super::UdpSocket).
    ///
    /// # Errors
    ///
    /// As `bind(2)`: the address is in use (shared only by sockets that all set
    /// `SO_REUSEPORT`, or on Windows `SO_REUSEADDR`), or not this host's.
    pub fn bind(self, addr: SocketAddr) -> io::Result<super::UdpSocket> {
        sys::bind_inet(self.raw(), &addr)?;
        let socket = std::net::UdpSocket::from(self.socket);
        Ok(Handle::from_nonblocking(socket, Kind::Udp))
    }
}

impl core::fmt::Debug for UdpSocketBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UdpSocketBuilder")
            .field("socket", &self.raw())
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl std::os::fd::AsRawFd for UdpSocketBuilder {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.raw()
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for UdpSocketBuilder {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.socket)
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawSocket for UdpSocketBuilder {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        self.raw()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsSocket for UdpSocketBuilder {
    fn as_socket(&self) -> std::os::windows::io::BorrowedSocket<'_> {
        std::os::windows::io::AsSocket::as_socket(&self.socket)
    }
}
