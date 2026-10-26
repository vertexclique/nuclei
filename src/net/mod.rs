//! Sockets: TCP, UDP and Unix domain, as [`Handle`]s.
//!
//! Each type keeps std's own socket inside; the handle adds the async
//! operations (`accept`, `connect`, `send`, `recv`, `peek`, `send_to`,
//! `recv_from`, `peek_from`) and, for the streams, `AsyncRead` and
//! `AsyncWrite` on the handle and on a shared reference to it, so two tasks
//! can read and write one stream at once. A stream's `read_owned` and
//! `write_owned` take the caller's buffer and give it back, so the driver
//! reads into it and writes from it with no copy.
//!
//! An address is anything [`ToSocketAddrs`] takes: numbers are used at
//! once; a name (`"localhost:80"`) is looked up on the blocking pool, so no
//! shard waits on the resolver. [`lookup_host`] looks one up alone.
//!
//! ```
//! use nuclei::net::{TcpListener, TcpStream};
//!
//! # nuclei::block_on(async {
//! let listener = TcpListener::bind("127.0.0.1:0").await?;
//! let addr = listener.get_ref().local_addr()?;
//! let client = TcpStream::connect(addr).await?;
//! let (server, _) = listener.accept().await?;
//! client.send(b"hi").await?;
//! let mut buf = [0u8; 2];
//! assert_eq!(server.recv(&mut buf).await?, 2);
//! # std::io::Result::Ok(())
//! # }).unwrap();
//! ```

mod addr;
mod opt;
mod socket;
mod stream;
mod tcp;
mod udp;
#[cfg(unix)]
pub mod unix;
#[cfg(unix)]
mod unix_socket;

pub use self::addr::{ToSocketAddrs, lookup_host};
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use self::socket::DeviceName;
pub use self::socket::TcpSocket;
pub use self::tcp::TcpStreamGenerator;
#[cfg(unix)]
pub use self::unix_socket::UnixSocket;

use std::io;

use crate::io::Handle;

/// A TCP listener: std's, with an async `accept`.
pub type TcpListener = Handle<std::net::TcpListener>;
/// A TCP stream: std's, with async reads and writes.
pub type TcpStream = Handle<std::net::TcpStream>;
/// A UDP socket: std's, with async sends and receives.
pub type UdpSocket = Handle<std::net::UdpSocket>;
/// A Unix stream listener: std's, with an async `accept`.
#[cfg(unix)]
pub type UnixListener = Handle<std::os::unix::net::UnixListener>;
/// A Unix stream: std's, with async reads and writes.
#[cfg(unix)]
pub type UnixStream = Handle<std::os::unix::net::UnixStream>;
/// A Unix datagram socket: std's, with async sends and receives.
#[cfg(unix)]
pub type UnixDatagram = Handle<std::os::unix::net::UnixDatagram>;

/// A port nobody listens on, so a connect to it is refused. On Linux a
/// socket bound to it and not listening holds it, so no test running
/// meanwhile is handed the port; macOS drops a connect to such a socket
/// rather than refusing it, and hands out ports in turn, so there (and on
/// Windows) the port is bound and freed again. The socket, if any, is the
/// caller's to keep while it connects.
#[cfg(test)]
pub(crate) fn refused_addr() -> (Option<TcpSocket>, std::net::SocketAddr) {
    if cfg!(any(target_os = "linux", target_os = "android")) {
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = socket.local_addr().unwrap();
        return (Some(socket), addr);
    }
    let freed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    (None, freed.local_addr().unwrap())
}

/// The error when every address failed (the last one's), or none was given.
fn none_worked(last: Option<io::Error>) -> io::Error {
    last.unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address was given"))
}

#[cfg(test)]
mod tests;
