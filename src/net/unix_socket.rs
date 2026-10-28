//! [`UnixSocket`]: a Unix domain socket made and bound before it becomes a
//! [`UnixStream`](super::UnixStream) (`connect`), a
//! [`UnixListener`](super::UnixListener) (`listen`) or a
//! [`UnixDatagram`](super::UnixDatagram) (`datagram`).
//!
//! As [`TcpSocket`](super::TcpSocket), it is its descriptor alone.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixListener};
use std::path::Path;

use crate::io::source::Kind;
use crate::io::{Handle, sys};

/// A Unix domain socket, of streams or of datagrams, to bind before it
/// connects, listens or sends.
///
/// ```
/// use nuclei::net::UnixSocket;
///
/// # nuclei::block_on(async {
/// let dir = std::env::temp_dir().join(format!("nuclei-doc-unix-socket-{}", std::process::id()));
/// let _ = std::fs::remove_file(&dir);
/// let listener = UnixSocket::new_stream()?;
/// listener.bind(&dir)?;
/// let listener = listener.listen(16)?;
/// let client = UnixSocket::new_stream()?.connect(&dir).await?;
/// let (_server, _) = listener.accept().await?;
/// client.send(b"hi").await?;
/// std::fs::remove_file(&dir)?;
/// # std::io::Result::Ok(())
/// # }).unwrap();
/// ```
pub struct UnixSocket {
    socket: OwnedFd,
    datagrams: bool,
}

impl UnixSocket {
    /// A new socket of streams.
    ///
    /// # Errors
    ///
    /// When the system has no socket to give.
    pub fn new_stream() -> io::Result<UnixSocket> {
        Self::new(false)
    }

    /// A new socket of datagrams.
    ///
    /// # Errors
    ///
    /// When the system has no socket to give.
    pub fn new_datagram() -> io::Result<UnixSocket> {
        Self::new(true)
    }

    fn new(datagrams: bool) -> io::Result<UnixSocket> {
        let ty = if datagrams {
            libc::SOCK_DGRAM
        } else {
            libc::SOCK_STREAM
        };
        Ok(UnixSocket {
            socket: sys::socket(libc::AF_UNIX, ty)?,
            datagrams,
        })
    }

    /// Binds the socket to `path`, which must not exist yet.
    ///
    /// # Errors
    ///
    /// As `bind(2)`: the path exists, or is too long.
    pub fn bind(&self, path: impl AsRef<Path>) -> io::Result<()> {
        sys::bind_unix(
            self.socket.as_raw_fd(),
            &sys::UnixAddr::path(path.as_ref())?,
        )
    }

    /// Makes a socket of streams a listener, with room for `backlog`
    /// connections not accepted yet.
    ///
    /// # Errors
    ///
    /// On a socket of datagrams, and as `listen(2)`.
    pub fn listen(self, backlog: u32) -> io::Result<super::UnixListener> {
        if self.datagrams {
            return Err(wrong_kind("a socket of datagrams cannot listen"));
        }
        sys::listen(self.socket.as_raw_fd(), backlog)?;
        Ok(Handle::from_nonblocking(
            UnixListener::from(self.socket),
            Kind::UnixListener,
        ))
    }

    /// Connects a socket of streams to the listener at `path`.
    ///
    /// # Errors
    ///
    /// On a socket of datagrams, and as `connect(2)`.
    pub async fn connect(self, path: impl AsRef<Path>) -> io::Result<super::UnixStream> {
        if self.datagrams {
            return Err(wrong_kind(
                "a socket of datagrams connects as a datagram socket",
            ));
        }
        super::unix::connect(self.socket, &sys::UnixAddr::path(path.as_ref())?).await
    }

    /// Makes a socket of datagrams a datagram socket, bound or not.
    ///
    /// # Errors
    ///
    /// On a socket of streams.
    pub fn datagram(self) -> io::Result<super::UnixDatagram> {
        if !self.datagrams {
            return Err(wrong_kind("a socket of streams is not a datagram socket"));
        }
        Ok(Handle::from_nonblocking(
            UnixDatagram::from(self.socket),
            Kind::UnixDatagram,
        ))
    }
}

fn wrong_kind(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

impl core::fmt::Debug for UnixSocket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UnixSocket")
            .field("fd", &self.socket.as_raw_fd())
            .field("datagrams", &self.datagrams)
            .finish()
    }
}

impl AsRawFd for UnixSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

impl AsFd for UnixSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

impl IntoRawFd for UnixSocket {
    fn into_raw_fd(self) -> RawFd {
        self.socket.into_raw_fd()
    }
}

impl FromRawFd for UnixSocket {
    /// # Safety
    ///
    /// `fd` is an open, non-blocking Unix domain socket the caller owns and
    /// hands over.
    unsafe fn from_raw_fd(fd: RawFd) -> UnixSocket {
        // SAFETY: the caller's contract.
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut ty: libc::c_int = 0;
        let mut len = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: SO_TYPE is an int, written into our own.
        let r = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                core::ptr::from_mut(&mut ty).cast(),
                &mut len,
            )
        };
        UnixSocket {
            socket,
            datagrams: r == 0 && ty == libc::SOCK_DGRAM,
        }
    }
}
