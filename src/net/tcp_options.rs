//! TCP's own options: keepalive timing ([`TcpKeepalive`]) and the user timeout on a socket and
//! on a stream, the options a server sets before it listens (the CPU incoming connections are
//! steered to, fast open, deferred accepts), and quick acks on a stream.
//!
//! Each is one system call on the descriptor, made where it is asked. An option the system does
//! not have answers `Unsupported`, and changes nothing.

use std::io;
use std::net::TcpStream;
use std::time::Duration;

use super::TcpSocket;
use super::opt::{self, IPPROTO_TCP, Int, SO_KEEPALIVE, SOL_SOCKET};
use crate::io::raw::Raw;
use crate::io::{Handle, IoObject};

/// When an idle connection's keepalive probes start, how far apart they go, and how many
/// unanswered ones drop the connection. A field left `None` keeps the system's value.
///
/// Times are whole seconds, as the system takes them: a part of one counts as one more.
///
/// ```
/// use std::time::Duration;
/// use nuclei::net::{TcpKeepalive, TcpSocket};
///
/// let socket = TcpSocket::new_v4()?;
/// socket.set_keepalive_timing(TcpKeepalive {
///     time: Some(Duration::from_secs(60)),
///     interval: Some(Duration::from_secs(10)),
///     retries: Some(5),
/// })?;
/// assert!(socket.keepalive()?);
/// # std::io::Result::Ok(())
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpKeepalive {
    /// How long the connection is idle before the first probe.
    pub time: Option<Duration>,
    /// How long after an unanswered probe the next one goes.
    pub interval: Option<Duration>,
    /// How many unanswered probes drop the connection.
    pub retries: Option<u32>,
}

/// The options of the keepalive's idle time, interval and count, where the system has them.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd"
))]
const KEEPALIVE: Option<[Int; 3]> =
    Some([libc::TCP_KEEPIDLE, libc::TCP_KEEPINTVL, libc::TCP_KEEPCNT]);
#[cfg(target_vendor = "apple")]
const KEEPALIVE: Option<[Int; 3]> =
    Some([libc::TCP_KEEPALIVE, libc::TCP_KEEPINTVL, libc::TCP_KEEPCNT]);
#[cfg(windows)]
const KEEPALIVE: Option<[Int; 3]> = {
    use crate::syscore::windows as win;
    Some([win::TCP_KEEPALIVE, win::TCP_KEEPINTVL, win::TCP_KEEPCNT])
};
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
    target_vendor = "apple",
    windows
)))]
const KEEPALIVE: Option<[Int; 3]> = None;

/// `SO_INCOMING_CPU`, which libc does not name: 49 on every Linux but SPARC's.
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(target_arch = "sparc", target_arch = "sparc64"))
))]
const SO_INCOMING_CPU: Int = 49;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "sparc", target_arch = "sparc64")
))]
const SO_INCOMING_CPU: Int = 0x33;

const SECOND: Duration = Duration::from_secs(1);

/// `time` in whole seconds, as the option of `what` takes it.
fn secs(time: Duration, what: &str) -> io::Result<Int> {
    opt::int(opt::whole(time, SECOND), what)
}

/// Turns keepalive probes on, timed as `keepalive` says.
fn set_keepalive_timing(raw: Raw, keepalive: TcpKeepalive) -> io::Result<()> {
    let [idle, interval, count] =
        KEEPALIVE.ok_or_else(|| opt::unsupported("TCP keepalive timing"))?;
    if let Some(time) = keepalive.time {
        opt::set(raw, IPPROTO_TCP, idle, secs(time, "keepalive time")?)?;
    }
    if let Some(time) = keepalive.interval {
        opt::set(
            raw,
            IPPROTO_TCP,
            interval,
            secs(time, "keepalive interval")?,
        )?;
    }
    if let Some(retries) = keepalive.retries {
        opt::set_count(raw, IPPROTO_TCP, count, retries, "keepalive retry count")?;
    }
    opt::set_flag(raw, SOL_SOCKET, SO_KEEPALIVE, true)
}

/// The keepalive's timing, every field read from the system.
fn keepalive_timing(raw: Raw) -> io::Result<TcpKeepalive> {
    let [idle, interval, count] =
        KEEPALIVE.ok_or_else(|| opt::unsupported("TCP keepalive timing"))?;
    let secs = |name| opt::count(raw, IPPROTO_TCP, name).map(|s| Duration::from_secs(s.into()));
    Ok(TcpKeepalive {
        time: Some(secs(idle)?),
        interval: Some(secs(interval)?),
        retries: Some(opt::count(raw, IPPROTO_TCP, count)?),
    })
}

/// Sets `TCP_USER_TIMEOUT` to `timeout`, in whole milliseconds; `None` or zero leaves it to the
/// system.
fn set_user_timeout(raw: Raw, timeout: Option<Duration>) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let ms = timeout.map_or(0, |t| opt::whole(t, Duration::from_millis(1)));
        opt::set(
            raw,
            IPPROTO_TCP,
            libc::TCP_USER_TIMEOUT,
            opt::int(ms, "user timeout")?,
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = (raw, timeout);
        Err(opt::unsupported("TCP_USER_TIMEOUT"))
    }
}

/// `TCP_USER_TIMEOUT`, `None` where the system's own applies.
fn user_timeout(raw: Raw) -> io::Result<Option<Duration>> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let ms = opt::count(raw, IPPROTO_TCP, libc::TCP_USER_TIMEOUT)?;
        Ok((ms > 0).then(|| Duration::from_millis(ms.into())))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = raw;
        Err(opt::unsupported("TCP_USER_TIMEOUT"))
    }
}

/// The calls a socket to set up and a connected stream share.
macro_rules! tcp_options {
    ($($ty:ty => $raw:ident),* $(,)?) => {$(
        impl $ty {
            /// Turns keepalive probes on (`SO_KEEPALIVE`), timed as `keepalive` says
            /// (`TCP_KEEPIDLE`, `TCP_KEEPINTVL`, `TCP_KEEPCNT`; macOS calls the first
            /// `TCP_KEEPALIVE`).
            ///
            /// # Errors
            ///
            /// `Unsupported` where the system has no such options, `InvalidInput` for a value
            /// past what it takes, and what `setsockopt(2)` answers (a zero time or count).
            pub fn set_keepalive_timing(&self, keepalive: TcpKeepalive) -> io::Result<()> {
                set_keepalive_timing(self.$raw(), keepalive)
            }

            /// The keepalive's timing: the system's values where none was set.
            ///
            /// # Errors
            ///
            /// `Unsupported` where the system has no such options.
            pub fn keepalive_timing(&self) -> io::Result<TcpKeepalive> {
                keepalive_timing(self.$raw())
            }

            /// Sets `TCP_USER_TIMEOUT` (Linux): how long sent bytes may go unacknowledged before
            /// the connection is dropped, in whole milliseconds; `None` or zero leaves it to the
            /// system.
            ///
            /// # Errors
            ///
            /// `Unsupported` off Linux, `InvalidInput` for a time past what the system takes.
            pub fn set_user_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
                set_user_timeout(self.$raw(), timeout)
            }

            /// `TCP_USER_TIMEOUT` (Linux), `None` where the system's own applies.
            ///
            /// # Errors
            ///
            /// `Unsupported` off Linux.
            pub fn user_timeout(&self) -> io::Result<Option<Duration>> {
                user_timeout(self.$raw())
            }
        }
    )*};
}

tcp_options!(TcpSocket => raw, Handle<TcpStream> => raw_io);

impl TcpSocket {
    /// Sets `SO_INCOMING_CPU` (Linux): the CPU whose connections this listener takes among
    /// listeners sharing a port with `SO_REUSEPORT`, so each core accepts the connections its
    /// own queues received.
    ///
    /// # Errors
    ///
    /// `Unsupported` off Linux, `InvalidInput` for a CPU past what the system takes.
    pub fn set_incoming_cpu(&self, cpu: u32) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return opt::set_count(self.raw(), SOL_SOCKET, SO_INCOMING_CPU, cpu, "CPU");
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = cpu;
            Err(opt::unsupported("SO_INCOMING_CPU"))
        }
    }

    /// `SO_INCOMING_CPU` (Linux): the CPU set, or the last to handle the socket's packets;
    /// `None` before either.
    ///
    /// # Errors
    ///
    /// `Unsupported` off Linux.
    pub fn incoming_cpu(&self) -> io::Result<Option<u32>> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return opt::get::<Int>(self.raw(), SOL_SOCKET, SO_INCOMING_CPU)
            .map(|cpu| u32::try_from(cpu).ok());
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        Err(opt::unsupported("SO_INCOMING_CPU"))
    }

    /// Sets `TCP_FASTOPEN` on a socket that will listen: connections may bring data with their
    /// handshake. Linux keeps up to `queue` such handshakes waiting; macOS, FreeBSD and Windows
    /// take it as on (above zero) or off.
    ///
    /// # Errors
    ///
    /// `Unsupported` where the system has no fast open, `InvalidInput` for a queue past what
    /// it takes, and what `setsockopt(2)` answers where the system turned fast open off.
    pub fn set_fastopen(&self, queue: u32) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return opt::set_count(
            self.raw(),
            IPPROTO_TCP,
            libc::TCP_FASTOPEN,
            queue,
            "fast open queue",
        );
        #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
        return opt::set_flag(self.raw(), IPPROTO_TCP, libc::TCP_FASTOPEN, queue > 0);
        #[cfg(windows)]
        return opt::set_flag(
            self.raw(),
            IPPROTO_TCP,
            crate::syscore::windows::TCP_FASTOPEN,
            queue > 0,
        );
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd",
            windows
        )))]
        {
            let _ = queue;
            Err(opt::unsupported("TCP_FASTOPEN"))
        }
    }

    /// `TCP_FASTOPEN`: the queue's length on Linux, and 1 or 0 where the system takes it as on or
    /// off.
    ///
    /// # Errors
    ///
    /// `Unsupported` where the system has no fast open.
    pub fn fastopen(&self) -> io::Result<u32> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return opt::count(self.raw(), IPPROTO_TCP, libc::TCP_FASTOPEN);
        #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
        return opt::flag(self.raw(), IPPROTO_TCP, libc::TCP_FASTOPEN).map(u32::from);
        #[cfg(windows)]
        return opt::flag(
            self.raw(),
            IPPROTO_TCP,
            crate::syscore::windows::TCP_FASTOPEN,
        )
        .map(u32::from);
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd",
            windows
        )))]
        Err(opt::unsupported("TCP_FASTOPEN"))
    }

    /// Sets `TCP_DEFER_ACCEPT` (Linux) on a socket that will listen: a connection is handed to
    /// `accept` only once its first data came, or `wait` passed (whole seconds, which the
    /// kernel rounds up to its retransmission steps); `None` hands each over at once.
    ///
    /// # Errors
    ///
    /// `Unsupported` off Linux, `InvalidInput` for a time past what the system takes.
    pub fn set_defer_accept(&self, wait: Option<Duration>) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let secs = match wait {
                Some(wait) => secs(wait, "deferred accept's wait")?,
                None => 0,
            };
            opt::set(self.raw(), IPPROTO_TCP, libc::TCP_DEFER_ACCEPT, secs)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = wait;
            Err(opt::unsupported("TCP_DEFER_ACCEPT"))
        }
    }

    /// `TCP_DEFER_ACCEPT` (Linux): how long a connection with no data waits, as the kernel
    /// rounded it; `None` if connections are handed over at once.
    ///
    /// # Errors
    ///
    /// `Unsupported` off Linux.
    pub fn defer_accept(&self) -> io::Result<Option<Duration>> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let secs = opt::count(self.raw(), IPPROTO_TCP, libc::TCP_DEFER_ACCEPT)?;
            Ok((secs > 0).then(|| Duration::from_secs(secs.into())))
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        Err(opt::unsupported("TCP_DEFER_ACCEPT"))
    }
}

impl Handle<TcpStream> {
    /// Sets `TCP_QUICKACK` (Linux): with `true` the stream acknowledges at once rather than
    /// waiting to send it with data. The kernel turns it off again as it sees fit, so it is set
    /// again where it matters.
    ///
    /// # Errors
    ///
    /// `Unsupported` off Linux.
    pub fn set_quickack(&self, quickack: bool) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return opt::set_flag(self.raw_io(), IPPROTO_TCP, libc::TCP_QUICKACK, quickack);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = quickack;
            Err(opt::unsupported("TCP_QUICKACK"))
        }
    }

    /// `TCP_QUICKACK` (Linux): whether the stream acknowledges at once now.
    ///
    /// # Errors
    ///
    /// `Unsupported` off Linux.
    pub fn quickack(&self) -> io::Result<bool> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return opt::flag(self.raw_io(), IPPROTO_TCP, libc::TCP_QUICKACK);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        Err(opt::unsupported("TCP_QUICKACK"))
    }
}
