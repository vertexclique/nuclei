//! [`TcpSocket`]: a TCP socket made, its options set and bound before it
//! becomes a [`TcpStream`](super::TcpStream) (`connect`) or a
//! [`TcpListener`](super::TcpListener) (`listen`).
//!
//! A builder is its descriptor and nothing else: no allocation, each
//! setting one system call on it.

use core::mem::ManuallyDrop;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use super::opt;
use crate::io::raw::{Owned, Raw};
use crate::io::source::Kind;
use crate::io::{Handle, sys};

/// A TCP socket to set up before it connects or listens.
///
/// ```
/// use nuclei::net::TcpSocket;
///
/// # nuclei::block_on(async {
/// let socket = TcpSocket::new_v4()?;
/// socket.set_reuseaddr(true)?;
/// socket.bind("127.0.0.1:0".parse().unwrap())?;
/// let addr = socket.local_addr()?;
/// let listener = socket.listen(128)?;
///
/// let client = TcpSocket::new_v4()?;
/// client.set_nodelay(true)?;
/// let stream = client.connect(addr).await?;
/// let (_peer, _) = listener.accept().await?;
/// assert!(stream.nodelay()?);
/// # std::io::Result::Ok(())
/// # }).unwrap();
/// ```
pub struct TcpSocket {
    socket: Owned,
    /// Made non-blocking by nuclei; one taken from elsewhere is set so when
    /// it connects or listens.
    nonblocking: bool,
}

/// A network interface's name, as `bind_device` takes it: up to 16 bytes,
/// held in place.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DeviceName {
    bytes: [u8; libc::IFNAMSIZ],
    len: u8,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl core::ops::Deref for DeviceName {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl core::fmt::Debug for DeviceName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(self))
    }
}

impl TcpSocket {
    /// A new IPv4 TCP socket.
    ///
    /// # Errors
    ///
    /// When the system has no socket to give.
    pub fn new_v4() -> io::Result<TcpSocket> {
        Self::new(false)
    }

    /// A new IPv6 TCP socket.
    ///
    /// # Errors
    ///
    /// When the system has no socket to give.
    pub fn new_v6() -> io::Result<TcpSocket> {
        Self::new(true)
    }

    fn new(v6: bool) -> io::Result<TcpSocket> {
        Ok(TcpSocket {
            socket: super::tcp::new_socket(v6)?,
            nonblocking: true,
        })
    }

    fn raw(&self) -> Raw {
        raw_of(&self.socket)
    }

    /// The socket, seen as std's stream for the calls std makes.
    fn as_std(&self) -> ManuallyDrop<std::net::TcpStream> {
        // SAFETY: the socket is ours and outlives the view, which never
        // closes it.
        ManuallyDrop::new(unsafe { std_from_raw(self.raw()) })
    }

    /// Sets `SO_KEEPALIVE`: the system probes an idle connection.
    pub fn set_keepalive(&self, keepalive: bool) -> io::Result<()> {
        opt::set_flag(self.raw(), opt::SOL_SOCKET, opt::SO_KEEPALIVE, keepalive)
    }

    /// Whether `SO_KEEPALIVE` is set.
    pub fn keepalive(&self) -> io::Result<bool> {
        opt::flag(self.raw(), opt::SOL_SOCKET, opt::SO_KEEPALIVE)
    }

    /// Sets `SO_REUSEADDR`: the socket may bind an address still held by a
    /// connection that is closing.
    pub fn set_reuseaddr(&self, reuseaddr: bool) -> io::Result<()> {
        opt::set_flag(self.raw(), opt::SOL_SOCKET, opt::SO_REUSEADDR, reuseaddr)
    }

    /// Whether `SO_REUSEADDR` is set.
    pub fn reuseaddr(&self) -> io::Result<bool> {
        opt::flag(self.raw(), opt::SOL_SOCKET, opt::SO_REUSEADDR)
    }

    /// Sets `SO_REUSEPORT`: several sockets may bind the same address and
    /// port, and the system shares the connections among them.
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    pub fn set_reuseport(&self, reuseport: bool) -> io::Result<()> {
        opt::set_flag(self.raw(), opt::SOL_SOCKET, opt::SO_REUSEPORT, reuseport)
    }

    /// Whether `SO_REUSEPORT` is set.
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    pub fn reuseport(&self) -> io::Result<bool> {
        opt::flag(self.raw(), opt::SOL_SOCKET, opt::SO_REUSEPORT)
    }

    /// Sets the size of the system's send buffer (`SO_SNDBUF`); the system
    /// may round it (Linux doubles it).
    pub fn set_send_buffer_size(&self, size: u32) -> io::Result<()> {
        opt::set_size(self.raw(), opt::SOL_SOCKET, opt::SO_SNDBUF, size)
    }

    /// The size of the system's send buffer (`SO_SNDBUF`).
    pub fn send_buffer_size(&self) -> io::Result<u32> {
        opt::size(self.raw(), opt::SOL_SOCKET, opt::SO_SNDBUF)
    }

    /// Sets the size of the system's receive buffer (`SO_RCVBUF`); the
    /// system may round it (Linux doubles it).
    pub fn set_recv_buffer_size(&self, size: u32) -> io::Result<()> {
        opt::set_size(self.raw(), opt::SOL_SOCKET, opt::SO_RCVBUF, size)
    }

    /// The size of the system's receive buffer (`SO_RCVBUF`).
    pub fn recv_buffer_size(&self) -> io::Result<u32> {
        opt::size(self.raw(), opt::SOL_SOCKET, opt::SO_RCVBUF)
    }

    /// Sets `SO_LINGER`: with `Some(time)` a close waits up to `time` (whole
    /// seconds) for unsent data; with `None` it returns at once and the
    /// system sends what is left.
    pub fn set_linger(&self, linger: Option<Duration>) -> io::Result<()> {
        opt::set_linger_time(self.raw(), linger)
    }

    /// Sets `SO_LINGER` to zero: a close drops unsent data and resets the
    /// connection.
    pub fn set_zero_linger(&self) -> io::Result<()> {
        opt::set_linger_time(self.raw(), Some(Duration::ZERO))
    }

    /// `SO_LINGER`: how long a close waits for unsent data, `None` if it
    /// does not wait.
    pub fn linger(&self) -> io::Result<Option<Duration>> {
        opt::linger_time(self.raw())
    }

    /// Sets `TCP_NODELAY`: small writes go out at once, not gathered.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.as_std().set_nodelay(nodelay)
    }

    /// Whether `TCP_NODELAY` is set.
    pub fn nodelay(&self) -> io::Result<bool> {
        self.as_std().nodelay()
    }

    /// Sets the type of service of the socket's IPv4 packets (`IP_TOS`).
    pub fn set_tos(&self, tos: u32) -> io::Result<()> {
        let tos = opt::Int::try_from(tos).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the type of service is out of range",
            )
        })?;
        opt::set(self.raw(), opt::IPPROTO_IP, opt::IP_TOS, tos)
    }

    /// The type of service of the socket's IPv4 packets (`IP_TOS`).
    pub fn tos(&self) -> io::Result<u32> {
        opt::size(self.raw(), opt::IPPROTO_IP, opt::IP_TOS)
    }

    /// Sets the traffic class of the socket's IPv6 packets (`IPV6_TCLASS`).
    #[cfg(unix)]
    pub fn set_tclass_v6(&self, tclass: u32) -> io::Result<()> {
        let tclass = opt::Int::try_from(tclass).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the traffic class is out of range",
            )
        })?;
        opt::set(self.raw(), opt::IPPROTO_IPV6, opt::IPV6_TCLASS, tclass)
    }

    /// The traffic class of the socket's IPv6 packets (`IPV6_TCLASS`).
    #[cfg(unix)]
    pub fn tclass_v6(&self) -> io::Result<u32> {
        opt::size(self.raw(), opt::IPPROTO_IPV6, opt::IPV6_TCLASS)
    }

    /// Binds the socket to the network interface named `interface`
    /// (`SO_BINDTODEVICE`), or, with `None`, to none.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn bind_device(&self, interface: Option<&[u8]>) -> io::Result<()> {
        let name = interface.unwrap_or_default();
        // SAFETY: the option takes the name's bytes as they are.
        unsafe {
            opt::set_raw(
                self.raw(),
                opt::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.as_ptr(),
                name.len(),
            )
        }
    }

    /// The network interface the socket is bound to, if any
    /// (`SO_BINDTODEVICE`).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn device(&self) -> io::Result<Option<DeviceName>> {
        let mut name = DeviceName {
            bytes: [0; libc::IFNAMSIZ],
            len: 0,
        };
        // SAFETY: the kernel writes at most the buffer's length.
        let len = unsafe {
            opt::get_raw(
                self.raw(),
                opt::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.bytes.as_mut_ptr(),
                name.bytes.len(),
            )?
        };
        // The kernel counts the name's terminating zero.
        let len = name.bytes[..len.min(name.bytes.len())]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(len.min(name.bytes.len()));
        name.len = len as u8;
        Ok((len > 0).then_some(name))
    }

    /// The address the socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.as_std().local_addr()
    }

    /// The socket's pending error (`SO_ERROR`), taken.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.as_std().take_error()
    }

    /// Binds the socket to `addr`.
    ///
    /// # Errors
    ///
    /// As `bind(2)`: the address is in use, or not this host's.
    pub fn bind(&self, addr: SocketAddr) -> io::Result<()> {
        sys::bind_inet(self.raw(), &addr)
    }

    /// Connects the socket to `addr`, as a stream.
    ///
    /// # Errors
    ///
    /// As `connect(2)`: refused, unreachable, or timed out.
    pub async fn connect(self, addr: SocketAddr) -> io::Result<super::TcpStream> {
        let TcpSocket {
            socket,
            nonblocking,
        } = self;
        super::tcp::connect_socket(socket, nonblocking, addr).await
    }

    /// Makes the socket a listener, with room for `backlog` connections not
    /// accepted yet.
    ///
    /// # Errors
    ///
    /// As `listen(2)`; a socket not bound gets an address of the system's
    /// choosing on unix, and an error on Windows.
    pub fn listen(self, backlog: u32) -> io::Result<super::TcpListener> {
        sys::listen(self.raw(), backlog)?;
        let listener = std::net::TcpListener::from(self.socket);
        if self.nonblocking {
            Ok(Handle::from_nonblocking(listener, Kind::TcpListener))
        } else {
            Handle::adopt(listener, Kind::TcpListener)
        }
    }

    /// Takes std's stream over as a socket to set up, connected or not.
    pub fn from_std_stream(stream: std::net::TcpStream) -> TcpSocket {
        TcpSocket {
            socket: Owned::from(stream),
            nonblocking: false,
        }
    }
}

impl core::fmt::Debug for TcpSocket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TcpSocket")
            .field("socket", &self.raw())
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
fn raw_of(socket: &Owned) -> Raw {
    use std::os::fd::AsRawFd;
    socket.as_raw_fd()
}

#[cfg(windows)]
fn raw_of(socket: &Owned) -> Raw {
    use std::os::windows::io::AsRawSocket;
    socket.as_raw_socket()
}

/// std's stream over the socket `raw`.
///
/// # Safety
///
/// `raw` is an open socket, which the caller keeps from being closed twice.
#[cfg(unix)]
unsafe fn std_from_raw(raw: Raw) -> std::net::TcpStream {
    use std::os::fd::FromRawFd;
    // SAFETY: the caller's contract.
    unsafe { std::net::TcpStream::from_raw_fd(raw) }
}

/// std's stream over the socket `raw`.
///
/// # Safety
///
/// `raw` is an open socket, which the caller keeps from being closed twice.
#[cfg(windows)]
unsafe fn std_from_raw(raw: Raw) -> std::net::TcpStream {
    use std::os::windows::io::FromRawSocket;
    // SAFETY: the caller's contract.
    unsafe { std::net::TcpStream::from_raw_socket(raw) }
}

#[cfg(unix)]
mod fd {
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

    use super::TcpSocket;

    impl AsRawFd for TcpSocket {
        fn as_raw_fd(&self) -> RawFd {
            self.socket.as_raw_fd()
        }
    }

    impl AsFd for TcpSocket {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.socket.as_fd()
        }
    }

    impl FromRawFd for TcpSocket {
        /// # Safety
        ///
        /// `fd` is an open TCP socket the caller owns and hands over.
        unsafe fn from_raw_fd(fd: RawFd) -> TcpSocket {
            TcpSocket {
                // SAFETY: the caller's contract.
                socket: unsafe { OwnedFd::from_raw_fd(fd) },
                nonblocking: false,
            }
        }
    }

    impl IntoRawFd for TcpSocket {
        fn into_raw_fd(self) -> RawFd {
            self.socket.into_raw_fd()
        }
    }
}

#[cfg(windows)]
mod raw_socket {
    use std::os::windows::io::{
        AsRawSocket, AsSocket, BorrowedSocket, FromRawSocket, IntoRawSocket, OwnedSocket, RawSocket,
    };

    use super::TcpSocket;

    impl AsRawSocket for TcpSocket {
        fn as_raw_socket(&self) -> RawSocket {
            self.socket.as_raw_socket()
        }
    }

    impl AsSocket for TcpSocket {
        fn as_socket(&self) -> BorrowedSocket<'_> {
            self.socket.as_socket()
        }
    }

    impl FromRawSocket for TcpSocket {
        /// # Safety
        ///
        /// `socket` is an open, overlapped TCP socket the caller owns and
        /// hands over.
        unsafe fn from_raw_socket(socket: RawSocket) -> TcpSocket {
            TcpSocket {
                // SAFETY: the caller's contract.
                socket: unsafe { OwnedSocket::from_raw_socket(socket) },
                nonblocking: false,
            }
        }
    }

    impl IntoRawSocket for TcpSocket {
        fn into_raw_socket(self) -> RawSocket {
            self.socket.into_raw_socket()
        }
    }
}
