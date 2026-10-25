//! Sockets: TCP, UDP and Unix domain, as [`Handle`](crate::io::Handle)s.
//!
//! Each type keeps std's own socket inside; the handle adds the async
//! operations (`accept`, `connect`, `send`, `recv`, `peek`, `send_to`,
//! `recv_from`, `peek_from`) and, for the streams, `AsyncRead` and
//! `AsyncWrite` on the handle and on a shared reference to it, so two tasks
//! can read and write one stream at once. A stream's `read_owned` and
//! `write_owned` take the caller's buffer and give it back, so the driver
//! reads into it and writes from it with no copy.
//!
//! Addresses given by name (`"localhost:80"`) are looked up on the calling
//! thread, as std does: the lookup blocks that thread while it lasts.
//! Addresses given as numbers, or as `SocketAddr`, need no lookup.

mod stream;
mod tcp;
mod udp;
#[cfg(unix)]
mod unix;

pub use self::tcp::TcpStreamGenerator;

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};

/// Every address `addr` names, or the error of a name that names none.
// vertexia: a name is looked up on the calling thread (std's resolver, as the
// signatures take std's `ToSocketAddrs`, which is neither `Send` nor
// `'static`); an async resolver on the blocking pool if lookups by name
// prove common on shards.
fn addresses<A: ToSocketAddrs>(addr: A) -> io::Result<impl Iterator<Item = SocketAddr>> {
    addr.to_socket_addrs()
}

/// The error when every address failed (the last one's), or none was given.
fn none_worked(last: Option<io::Error>) -> io::Error {
    last.unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address was given"))
}

#[cfg(test)]
mod tests;
