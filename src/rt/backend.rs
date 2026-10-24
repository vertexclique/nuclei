//! Which kernel interface the process's shards drive their IO with.
//!
//! `NUCLEI_BACKEND` names it: `auto` (the same as leaving it unset), `io_uring` or `epoll`.
//! `auto` takes io_uring when the kernel lets this process make the ring its shards use (see
//! `syscore::linux::probe`, which checks without risking the process under a seccomp filter),
//! and the readiness poller (epoll, kqueue) otherwise. An explicit `io_uring` that the kernel
//! cannot give fails with the reason when the first shard starts, never falls back.

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
        .get_or_init(|| decide(&std::env::var("NUCLEI_BACKEND").unwrap_or_default(), usable))
        .clone()
        .map_err(|why| io::Error::new(io::ErrorKind::Unsupported, why))
}

/// Whether the process's shards run rings: every source but a regular
/// file then does its IO as ring operations, from its first one.
#[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
pub(crate) fn rings() -> bool {
    matches!(chosen(), Ok(Backend::Ring(_)))
}

/// Whether this process can use io_uring, or why not.
#[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
fn usable() -> Result<(), String> {
    use crate::syscore::linux::probe::{Probe, probe};
    match probe() {
        Probe::Available => Ok(()),
        Probe::Unavailable(why) => Err(why.to_string()),
    }
}

#[cfg(not(all(any(target_os = "linux", target_os = "android"), feature = "iouring")))]
fn usable() -> Result<(), String> {
    Err("this build has no io_uring".into())
}

/// The backend `named` asks for, given whether io_uring is usable here.
fn decide(named: &str, usable: impl FnOnce() -> Result<(), String>) -> Result<Backend, String> {
    match named {
        "epoll" => Ok(Backend::Poll),
        "" | "auto" => Ok(match usable() {
            Ok(()) => ring(),
            Err(_) => Backend::Poll,
        }),
        "io_uring" => match usable() {
            Ok(()) => Ok(ring()),
            Err(why) => Err(format!(
                "NUCLEI_BACKEND=io_uring, but {why}; unset it to use epoll"
            )),
        },
        other => Err(format!(
            "NUCLEI_BACKEND={other} names no backend (use auto, epoll or io_uring)"
        )),
    }
}

/// The ring the shards make, as the process's configuration says.
fn ring() -> Backend {
    #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
    return Backend::Ring((&crate::proactor::Proactor::get().config().iouring).into());
    #[cfg(not(all(any(target_os = "linux", target_os = "android"), feature = "iouring")))]
    Backend::Poll
}

#[cfg(test)]
mod tests {
    use super::{Backend, decide};

    fn usable() -> Result<(), String> {
        Ok(())
    }

    fn refused() -> Result<(), String> {
        Err("the system refused io_uring".into())
    }

    fn is_ring(b: &Result<Backend, String>) -> bool {
        #[cfg(all(any(target_os = "linux", target_os = "android"), feature = "iouring"))]
        return matches!(b, Ok(Backend::Ring(_)));
        #[cfg(not(all(any(target_os = "linux", target_os = "android"), feature = "iouring")))]
        {
            let _ = b;
            false
        }
    }

    /// Unset or `auto`: a ring where io_uring is usable, the poller where it is not.
    #[test]
    fn auto_takes_a_ring_first_and_falls_back_to_the_poller() {
        let ring_built = cfg!(all(
            any(target_os = "linux", target_os = "android"),
            feature = "iouring"
        ));
        for named in ["", "auto"] {
            assert_eq!(is_ring(&decide(named, usable)), ring_built, "{named:?}");
            assert!(
                matches!(decide(named, refused), Ok(Backend::Poll)),
                "{named:?}"
            );
        }
    }

    /// An explicit `io_uring` the kernel cannot give fails with the reason.
    #[test]
    fn an_explicit_ring_that_cannot_be_had_fails_with_the_reason() {
        let err = decide("io_uring", refused).unwrap_err();
        assert!(err.contains("the system refused io_uring"), "{err}");
        assert!(err.contains("unset it"), "{err}");
    }

    #[test]
    fn epoll_never_asks_about_io_uring_and_unknown_names_fail() {
        assert!(matches!(
            decide("epoll", || panic!("epoll asked about io_uring")),
            Ok(Backend::Poll)
        ));
        let err = decide("kqueue-ish", usable).unwrap_err();
        assert!(err.contains("NUCLEI_BACKEND=kqueue-ish"), "{err}");
    }

    /// This machine's answer, as the process takes it: with no backend named, its shards run
    /// rings exactly when io_uring is usable here.
    #[cfg(all(target_os = "linux", feature = "iouring"))]
    #[test]
    fn the_probe_decides_auto_on_this_machine() {
        use crate::syscore::linux::probe::{Probe, probe};
        let available = probe() == Probe::Available;
        assert_eq!(is_ring(&decide("auto", super::usable)), available);
        let named = std::env::var("NUCLEI_BACKEND").unwrap_or_default();
        if named.is_empty() || named == "auto" {
            assert_eq!(
                is_ring(&super::chosen().map_err(|e| e.to_string())),
                available
            );
        }
    }
}
