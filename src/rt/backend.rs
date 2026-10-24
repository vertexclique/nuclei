//! Which kernel interface the process's shards drive their IO with.
//!
//! `NUCLEI_BACKEND` names it: `io_uring`, `epoll`, or `auto` (the same as
//! leaving it unset). An explicit `io_uring` that the kernel cannot give
//! fails with the reason when the first shard starts, never falls back.
// vertexia: `auto` picks epoll until the ring path serves every operation on
// every kind of source; it then picks io_uring where the probe allows it.

use std::io;
use std::sync::OnceLock;

/// A shard's driver.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Backend {
    /// A readiness poller (epoll, kqueue): the driver makes the calls.
    Poll,
    /// An io_uring ring, made as configured: the sources' IO is its
    /// operations.
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    Ring(crate::syscore::linux::iouring::ring::RingConfig),
}

/// The backend the process's shards use, decided once.
pub(crate) fn chosen() -> io::Result<Backend> {
    static CHOSEN: OnceLock<Result<Backend, String>> = OnceLock::new();
    CHOSEN
        .get_or_init(decide)
        .clone()
        .map_err(|why| io::Error::new(io::ErrorKind::Unsupported, why))
}

/// Whether the process's shards run rings: every source but a regular
/// file then does its IO as ring operations, from its first one.
#[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
pub(crate) fn rings() -> bool {
    matches!(chosen(), Ok(Backend::Ring(_)))
}

fn decide() -> Result<Backend, String> {
    let named = std::env::var("NUCLEI_BACKEND").unwrap_or_default();
    match named.as_str() {
        "" | "auto" | "epoll" => Ok(Backend::Poll),
        #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
        "io_uring" => Ok(Backend::Ring(
            (&crate::proactor::Proactor::get().config().iouring).into(),
        )),
        other => Err(format!(
            "NUCLEI_BACKEND={other} names no backend this build has (use auto, epoll or io_uring)"
        )),
    }
}
