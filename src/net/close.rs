//! A stream's ends: [`shut`](crate::net::TcpStream::shut), std's shutdown made once the bytes
//! written behind reached the kernel, and [`abort`](crate::net::TcpStream::abort), a TCP
//! stream's close that resets the connection now (`proofs/tla/close`).
//!
//! The shut is not named `shutdown`: std's own, which a handle reaches through `Deref`, keeps
//! that name and its meaning, and an inherent method of that name would take its place in every
//! caller's `stream.shutdown(how)`, a future where a result was.

use core::future::poll_fn;
use std::io;
use std::net::{Shutdown, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use crate::io::source::Source;
use crate::io::{Handle, IoObject, sys};

/// Whether the kernel drops what its receive queue holds as it shuts the reading half (the
/// BSDs, macOS among them, and Windows), so the handle drops what it read ahead too. Linux keeps
/// the queue readable, and the read-ahead with it.
const KERNEL_DROPS_QUEUE: bool = !cfg!(any(target_os = "linux", target_os = "android"));

macro_rules! shut {
    ($($ty:ty),* $(,)?) => {$(
        impl Handle<$ty> {
            /// Shuts down the reading half of the stream, its writing half, or both, as std's
            /// `shutdown` does, once the bytes written behind reached the kernel.
            ///
            /// `Write` and `Both` wait until the kernel took every byte the handle accepted, so
            /// the peer reads all of them before the end of the stream, then ask the kernel; a
            /// peer that never reads holds them up, as it would hold up std's blocking writes.
            /// `Read` leaves the writes alone. After `Read` or `Both`, reads answer as std's do:
            /// Linux hands out what came before the shutdown, then the end; macOS and Windows end
            /// at once, so there the handle drops what it read ahead.
            ///
            /// It is not named `shutdown`, so that `stream.shutdown(how)` still calls std's own,
            /// reached through `Deref`, which shuts the halves at once: the bytes still behind
            /// then never reach the peer, and it reads the end before them.
            ///
            /// # Errors
            ///
            /// The error that stopped the bytes written behind, and what `shutdown(2)` answers:
            /// a half shut already on macOS (`NotConnected`), as std's.
            pub async fn shut(&self, how: Shutdown) -> io::Result<()> {
                shut(self.source(), how).await
            }
        }
    )*};
}

shut!(TcpStream);
#[cfg(unix)]
shut!(UnixStream);

/// [`Handle::shut`] of the stream whose source is `source`.
async fn shut(source: &Arc<Source>, how: Shutdown) -> io::Result<()> {
    match how {
        Shutdown::Read => shut_read(source).await?,
        Shutdown::Write | Shutdown::Both => {
            poll_fn(|cx| source.poll_shutdown(cx, how)).await?;
        }
    }
    if how != Shutdown::Write && KERNEL_DROPS_QUEUE {
        poll_fn(|cx| source.poll_drop_ahead(cx)).await?;
    }
    Ok(())
}

/// The kernel's shutdown of the reading half: the ring's entry on a thread whose shard runs a
/// ring, else the call itself. The lease keeps the descriptor's number the stream's until the
/// call ran.
async fn shut_read(source: &Arc<Source>) -> io::Result<()> {
    let lease = source.lease()?;
    #[cfg(ring)]
    if let Ok(mut job) = crate::fs::ring::shutdown(&lease, libc::SHUT_RD) {
        return poll_fn(|cx| job.poll(cx.waker()))
            .await
            .inspect_err(crate::rt::counters::io_error);
    }
    sys::shutdown(lease.raw(), Shutdown::Read).inspect_err(crate::rt::counters::io_error)
}

impl Handle<TcpStream> {
    /// Closes the stream now and resets the connection: `SO_LINGER` goes to zero, the bytes
    /// written behind and the buffers lent to writes still out are dropped, nothing lingers, and
    /// the close sends the peer a reset, so its reads fail with `ConnectionReset` rather than end
    /// as if every byte came. The kernel drops what its own queue holds too.
    ///
    /// A plain drop instead delivers the bytes behind, for at most the runtime's
    /// [`write_behind_linger`](crate::runtime::Builder::write_behind_linger), and ends the
    /// stream cleanly; one whose time runs out resets the connection the same way.
    ///
    /// # Errors
    ///
    /// When `SO_LINGER` cannot be set: the stream then closes as a drop closes it.
    pub fn abort(self) -> io::Result<()> {
        super::opt::set_linger_time(self.raw_io(), Some(Duration::ZERO))?;
        self.close_now();
        Ok(())
    }
}
