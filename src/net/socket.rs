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

super::sockopt::socket_options!(TcpSocket);

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

    pub(super) fn raw(&self) -> Raw {
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
