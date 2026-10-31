//! Sockets: TCP, UDP and Unix domain, as [`Handle`]s.
//!
//! Each type keeps std's own socket inside; the handle adds the async
//! operations (`accept`, `connect`, `send`, `recv`, `peek`, `send_to`,
//! `recv_from`, `peek_from`) and, for the streams, `AsyncRead` and
//! `AsyncWrite` on the handle and on a shared reference to it, so two tasks
//! can read and write one stream at once. A stream's `read_owned` and
//! `write_owned` take the caller's buffer and give it back, so the driver
//! reads into it and writes from it with no copy; a TCP stream's
//! `write_zero_copy` asks the kernel not to copy it either, and gives it back
//! through a [`Released`] once the kernel let go of it.
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
mod close;
pub(crate) mod opt;
mod released;
pub(crate) mod send_file;
mod socket;
mod sockopt;
mod split;
mod stream;
mod tcp;
mod tcp_options;
mod udp;
mod udp_socket;
#[cfg(unix)]
pub mod unix;
#[cfg(unix)]
mod unix_socket;

pub use self::addr::{ToSocketAddrs, lookup_host};
pub use self::released::Released;
pub use self::socket::TcpSocket;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use self::sockopt::DeviceName;
pub use self::split::{OwnedReadHalf, OwnedWriteHalf, ReadHalf, ReuniteError, WriteHalf};
pub use self::tcp::TcpStreamGenerator;
pub use self::tcp_options::TcpKeepalive;
pub use self::udp_socket::UdpSocketBuilder;
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

/// One poll of `poll`, outside the task's budget, with no waker kept: what
/// a `try_` call answers, `WouldBlock` for one that would wait (which still
/// sets the driver going, so a later call finds what came).
fn at_once<T>(
    poll: impl FnOnce(&mut core::task::Context<'_>) -> core::task::Poll<io::Result<T>>,
) -> io::Result<T> {
    crate::coop::outside(|| {
        let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
        match poll(&mut cx) {
            core::task::Poll::Ready(result) => result,
            core::task::Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    })
}

/// What `interest` asks for of `source`, once some of it is ready.
fn poll_ready(
    source: &std::sync::Arc<crate::io::Source>,
    cx: &mut core::task::Context<'_>,
    interest: crate::io::Interest,
) -> core::task::Poll<io::Result<crate::io::Ready>> {
    use crate::io::Ready;
    use core::task::Poll;
    let mut ready = Ready::EMPTY;
    if interest.is_readable()
        && let Poll::Ready(closed) = source.poll_read_ready(cx)
    {
        ready = ready | Ready::READABLE;
        if closed? {
            ready = ready | Ready::READ_CLOSED;
        }
    }
    if interest.is_writable()
        && let Poll::Ready(closed) = source.poll_write_ready(cx)
    {
        ready = ready | Ready::WRITABLE;
        if closed? {
            ready = ready | Ready::WRITE_CLOSED;
        }
    }
    if ready.is_empty() {
        Poll::Pending
    } else {
        Poll::Ready(Ok(ready))
    }
}

/// A socket's calls of its caller's own: `try_io` and `async_io`.
macro_rules! caller_io {
    ($($ty:ty),* $(,)?) => {$(
        #[cfg(unix)]
        impl Handle<$ty> {
            /// Runs `f`, a call of the caller's own on the socket (through
            /// [`get_ref`](Handle::get_ref) or its descriptor), if the socket
            /// may be ready for `interest`, and clears that readiness if `f`
            /// answers `WouldBlock`. The side then takes its caller's calls
            /// only, until the handle's own next read or write on it. Nothing
            /// is reordered: bytes the handle wrote behind go out first, and
            /// data it read ahead must be read before.
            ///
            /// # Errors
            ///
            /// `WouldBlock` when none of `interest` may be ready (or bytes
            /// written behind are still going out), `ResourceBusy` when the
            /// handle holds data it read ahead, and what `f` answers.
            pub fn try_io<R>(
                &self,
                interest: crate::io::Interest,
                f: impl FnOnce() -> io::Result<R>,
            ) -> io::Result<R> {
                crate::io::caller::try_io(self.source(), crate::io::caller::sides(interest), f)
            }

            /// Runs `f`, a call of the caller's own on the socket, each time
            /// it may be ready for `interest`, until `f` answers other than
            /// `WouldBlock`, clearing the readiness each time it does; as
            /// [`try_io`](Self::try_io), bytes written behind go out first.
            ///
            /// # Errors
            ///
            /// `ResourceBusy` when the handle holds data it read ahead, and
            /// what `f` answers.
            pub async fn async_io<R>(
                &self,
                interest: crate::io::Interest,
                f: impl FnMut() -> io::Result<R>,
            ) -> io::Result<R> {
                crate::io::caller::async_io(self.source(), crate::io::caller::sides(interest), f)
                    .await
            }
        }
    )*};
}

caller_io!(std::net::TcpStream, std::net::UdpSocket);
#[cfg(unix)]
caller_io!(
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixDatagram
);

/// The error when every address failed (the last one's), or none was given.
fn none_worked(last: Option<io::Error>) -> io::Error {
    last.unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address was given"))
}

#[cfg(test)]
mod tests;
