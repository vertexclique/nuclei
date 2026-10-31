//! Which kernel interface a runtime's shards drive their IO with.
//!
//! A program names it with [`Builder::backend`](crate::Builder::backend)
//! for one runtime, or with the `backend` field of
//! [`NucleiConfig`](crate::config::NucleiConfig) for every runtime that
//! names none; `NUCLEI_BACKEND` (`auto`, `io_uring`, `epoll`, `kqueue`,
//! `iocp`), when set, overrides both. Named by nobody, it is `auto`:
//! io_uring when the kernel lets this process make the ring its shards use
//! (see `syscore::linux::probe`, which checks without risking the process
//! under a seccomp filter, and `rt::shard::ring::check`, which builds that
//! ring as a shard does so every feature it needs is known to be there: the
//! buffer ring for multishot receives while `recv_buffers` is not 0, the
//! table of fixed files), and the platform's own poller otherwise (its I/O
//! completion port on Windows).
//! A backend named explicitly that this process cannot have fails with the
//! reason when the first shard starts, and never falls back.

use std::io;
use std::sync::OnceLock;

use crate::IoBackend;

/// A shard's driver.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Backend {
    /// The platform's readiness poller (epoll, kqueue): the driver makes the
    /// calls.
    Poll,
    /// An io_uring ring, made as configured: the sources' IO is its
    /// operations. A shard that cannot make it fails.
    #[cfg(ring)]
    Ring(crate::syscore::linux::iouring::ring::RingConfig),
    /// A ring as `Ring`, chosen by `auto`: a shard whose ring finds no
    /// memory even with fewer entries runs the poller instead.
    #[cfg(ring)]
    RingOrPoll(crate::syscore::linux::iouring::ring::RingConfig),
}

impl Backend {
    /// The backend for a worker shard that may be handed to another thread:
    /// its ring (if it runs one) takes submissions from any thread.
    pub(crate) fn handed_over(self) -> Self {
        match self {
            #[cfg(ring)]
            Backend::Ring(cfg) => Backend::Ring(cfg.handed_over()),
            #[cfg(ring)]
            Backend::RingOrPoll(cfg) => Backend::RingOrPoll(cfg.handed_over()),
            other => other,
        }
    }

    /// Whether shards on this backend run rings.
    pub(crate) fn rings(self) -> bool {
        #[cfg(ring)]
        return matches!(self, Backend::Ring(_) | Backend::RingOrPoll(_));
        #[cfg(not(ring))]
        {
            let _ = self;
            false
        }
    }
}

/// The backend of every runtime that names none: the process's default,
/// decided once.
pub(crate) fn chosen() -> io::Result<Backend> {
    static CHOSEN: OnceLock<Result<Backend, String>> = OnceLock::new();
    CHOSEN
        .get_or_init(|| {
            let configured = crate::proactor::Proactor::get().config().backend;
            decide(env_override().as_deref(), configured, usable)
        })
        .clone()
        .map_err(unsupported)
        .and_then(checked)
}

/// The backend of a runtime that names `named`: the process's default if it
/// names none, or if `NUCLEI_BACKEND` overrides it.
pub(crate) fn for_runtime(named: Option<IoBackend>) -> io::Result<Backend> {
    match (env_override(), named) {
        (None, Some(named)) => decide(None, Some(named), usable)
            .map_err(unsupported)
            .and_then(checked),
        _ => chosen(),
    }
}

/// `backend`, once the process's io_uring sizes are known to be ones a ring
/// can work with: a configuration's mistake is reported where it takes
/// effect, never run with. Only a ring backend reads them.
fn checked(backend: Backend) -> io::Result<Backend> {
    #[cfg(ring)]
    if matches!(backend, Backend::Ring(_) | Backend::RingOrPoll(_)) {
        crate::proactor::Proactor::get()
            .config()
            .iouring
            .validate()?;
    }
    Ok(backend)
}

fn unsupported(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, why)
}

/// `NUCLEI_BACKEND`, when set to something.
fn env_override() -> Option<String> {
    std::env::var("NUCLEI_BACKEND")
        .ok()
        .filter(|b| !b.is_empty())
}

/// Whether this process can use io_uring (the checks run once), or why not.
#[cfg(ring)]
fn usable() -> Result<(), String> {
    static PROBED: OnceLock<Result<(), String>> = OnceLock::new();
    PROBED.get_or_init(|| usable_with(ring_config())).clone()
}

/// [`usable`], not remembered: whether a ring can be made here at all
/// (`probe`), and then whether the kernel gives a shard of `cfg` all its ring
/// needs (`check`), as a shard that stays on its thread runs it and as one
/// that may be handed over does.
///
/// Decided on a thread of its own: tearing a ring down makes the kernel queue
/// io_uring task work, with a signal, on every thread that made or used it,
/// which cuts that thread's next blocking wait short (`EINTR`); and a ring
/// one thread enables registers in that thread's table of ring fds. The
/// caller's thread makes no ring of its own this way.
#[cfg(ring)]
pub(crate) fn usable_with(
    cfg: crate::syscore::linux::iouring::ring::RingConfig,
) -> Result<(), String> {
    let apart = std::thread::Builder::new()
        .name("nuclei-check".into())
        .spawn(move || usable_here(cfg));
    match apart {
        Ok(thread) => thread
            .join()
            .unwrap_or_else(|_| Err("the io_uring check panicked".into())),
        // No thread to spare: decided here, side effects and all.
        Err(_) => usable_here(cfg),
    }
}

/// [`usable_with`] on the calling thread.
#[cfg(ring)]
fn usable_here(cfg: crate::syscore::linux::iouring::ring::RingConfig) -> Result<(), String> {
    use crate::rt::shard::ring::check;
    use crate::syscore::linux::probe::{Probe, probe};
    match probe() {
        Probe::Available => {}
        Probe::Unavailable(why) => return Err(why.to_string()),
    }
    for cfg in [cfg, cfg.handed_over()] {
        match check(cfg) {
            Ok(()) => {}
            // Short of memory is no missing feature: a shard makes its ring
            // with less memory on its own (`with_room`), and starts without a
            // receive group it has no memory for (`recv::went_without`). A
            // configuration mistake is the shard's to report too, not a
            // reason to run another backend unseen.
            Err(err)
                if err.kind() == io::ErrorKind::OutOfMemory
                    || err.kind() == io::ErrorKind::InvalidInput => {}
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(())
}

#[cfg(not(ring))]
fn usable() -> Result<(), String> {
    Err("this build has no io_uring".into())
}

/// What is asked for, whoever asked.
enum Ask {
    Auto,
    Ring,
    Poller,
}

/// The backend asked for by `env` (`NUCLEI_BACKEND`), else by `configured`
/// (the program), given whether io_uring is usable here.
fn decide(
    env: Option<&str>,
    configured: Option<IoBackend>,
    usable: impl FnOnce() -> Result<(), String>,
) -> Result<Backend, String> {
    let (ask, named) = match env {
        Some(name) => (ask_by_name(name)?, format!("NUCLEI_BACKEND={name}")),
        None => match configured {
            None => (Ask::Auto, String::new()),
            Some(backend) => (ask_for(backend)?, format!("the backend {backend:?}")),
        },
    };
    match ask {
        Ask::Poller => Ok(Backend::Poll),
        Ask::Auto => Ok(match usable() {
            Ok(()) => ring(true),
            Err(_) => Backend::Poll,
        }),
        Ask::Ring => match usable() {
            Ok(()) => Ok(ring(false)),
            Err(why) => Err(format!(
                "{named} asks for io_uring, but {why}; leave it unset to use the poller"
            )),
        },
    }
}

/// The platform's own poller (on Windows, its completion port), by name.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "illumos"))]
const POLLER: (&str, IoBackend) = ("epoll", IoBackend::Epoll);
#[cfg(windows)]
const POLLER: (&str, IoBackend) = ("iocp", IoBackend::Iocp);
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "illumos",
    windows
)))]
const POLLER: (&str, IoBackend) = ("kqueue", IoBackend::Kqueue);

fn ask_by_name(name: &str) -> Result<Ask, String> {
    match name {
        "auto" => Ok(Ask::Auto),
        "io_uring" => Ok(Ask::Ring),
        poller if poller == POLLER.0 => Ok(Ask::Poller),
        other => Err(format!(
            "NUCLEI_BACKEND={other} names no backend of this platform (use auto, {}{})",
            POLLER.0,
            if cfg!(any(target_os = "linux", target_os = "android")) {
                " or io_uring"
            } else {
                ""
            }
        )),
    }
}

fn ask_for(backend: IoBackend) -> Result<Ask, String> {
    match backend {
        IoBackend::IoUring => Ok(Ask::Ring),
        poller if poller == POLLER.1 => Ok(Ask::Poller),
        other => Err(format!("this platform has no {other:?} backend")),
    }
}

/// The ring configuration of a shard of a runtime that names no backend: the
/// process's, as `Proactor::with_config` set it. The one place it is derived,
/// for the shards and for the check that chooses their backend.
#[cfg(ring)]
fn ring_config() -> crate::syscore::linux::iouring::ring::RingConfig {
    (&crate::proactor::Proactor::get().config().iouring).into()
}

/// The ring the shards make, as the process's configuration says; `auto`:
/// whether `auto` chose it.
fn ring(auto: bool) -> Backend {
    #[cfg(ring)]
    {
        let cfg = ring_config();
        if auto {
            Backend::RingOrPoll(cfg)
        } else {
            Backend::Ring(cfg)
        }
    }
    #[cfg(not(ring))]
    {
        let _ = auto;
        Backend::Poll
    }
}

/// Makes something of `entries` entries with `make`, halving them while it
/// finds no memory, down to `least`: a ring under a tight memory limit gets
/// what fits rather than nothing.
#[cfg(ring)]
pub(crate) fn with_room<T>(
    entries: u32,
    least: u32,
    mut make: impl FnMut(u32) -> io::Result<T>,
) -> io::Result<T> {
    let mut entries = entries.max(least);
    loop {
        match make(entries) {
            Err(err) if err.raw_os_error() == Some(libc::ENOMEM) && entries > least => {
                entries = (entries / 2).max(least);
            }
            result => return result,
        }
    }
}

#[cfg(all(test, ring, target_os = "linux"))]
mod tests_refused;

#[cfg(test)]
mod tests {
    #[cfg(ring)]
    use super::with_room;
    use super::{Backend, IoBackend, decide};

    const RINGS_BUILT: bool = cfg!(ring);

    fn usable() -> Result<(), String> {
        Ok(())
    }

    fn refused() -> Result<(), String> {
        Err("the system refused io_uring".into())
    }

    fn is_ring(b: &Result<Backend, String>) -> bool {
        b.as_ref().is_ok_and(|b| b.rings())
    }

    /// Nothing named, or `auto`: a ring where io_uring is usable, the poller
    /// where it is not.
    #[test]
    fn auto_takes_a_ring_first_and_falls_back_to_the_poller() {
        for env in [None, Some("auto")] {
            assert_eq!(is_ring(&decide(env, None, usable)), RINGS_BUILT, "{env:?}");
            assert!(
                matches!(decide(env, None, refused), Ok(Backend::Poll)),
                "{env:?}"
            );
        }
    }

    /// Only a ring `auto` chose may fall back for one shard; an explicit one
    /// may not.
    #[cfg(ring)]
    #[test]
    fn only_autos_ring_may_fall_back() {
        assert!(matches!(
            decide(None, None, usable),
            Ok(Backend::RingOrPoll(_))
        ));
        assert!(matches!(
            decide(Some("io_uring"), None, usable),
            Ok(Backend::Ring(_))
        ));
        assert!(matches!(
            decide(None, Some(IoBackend::IoUring), usable),
            Ok(Backend::Ring(_))
        ));
    }

    /// An explicit io_uring, named either way, that cannot be had fails with
    /// the reason.
    #[test]
    fn an_explicit_ring_that_cannot_be_had_fails_with_the_reason() {
        for (env, configured) in [(Some("io_uring"), None), (None, Some(IoBackend::IoUring))] {
            let err = decide(env, configured, refused).unwrap_err();
            assert!(err.contains("the system refused io_uring"), "{err}");
            assert!(err.contains("leave it unset"), "{err}");
        }
    }

    /// The variable overrides what the program configured.
    #[test]
    fn the_variable_overrides_the_program() {
        let poller = super::POLLER;
        assert!(matches!(
            decide(Some(poller.0), Some(IoBackend::IoUring), usable),
            Ok(Backend::Poll)
        ));
        assert_eq!(
            is_ring(&decide(Some("auto"), Some(poller.1), usable)),
            RINGS_BUILT
        );
    }

    #[test]
    fn the_poller_never_asks_about_io_uring_and_foreign_names_fail() {
        let poller = super::POLLER;
        let never = || -> Result<(), String> { panic!("the poller asked about io_uring") };
        assert!(matches!(
            decide(Some(poller.0), None, never),
            Ok(Backend::Poll)
        ));
        assert!(matches!(
            decide(None, Some(poller.1), never),
            Ok(Backend::Poll)
        ));
        let err = decide(Some("kqueue-ish"), None, usable).unwrap_err();
        assert!(err.contains("NUCLEI_BACKEND=kqueue-ish"), "{err}");
        for foreign in [IoBackend::Epoll, IoBackend::Kqueue, IoBackend::Iocp] {
            if foreign != poller.1 {
                let err = decide(None, Some(foreign), usable).unwrap_err();
                assert!(err.contains("this platform has no"), "{err}");
            }
        }
    }

    #[cfg(ring)]
    #[test]
    fn a_ring_short_of_memory_is_made_smaller_down_to_the_least() {
        let mut tried = Vec::new();
        let made = with_room(1024, 64, |n| {
            tried.push(n);
            if n > 200 {
                Err(std::io::Error::from_raw_os_error(libc::ENOMEM))
            } else {
                Ok(n)
            }
        });
        assert_eq!(made.unwrap(), 128);
        assert_eq!(tried, [1024, 512, 256, 128]);
        // Memory short even at the least, and any other error, come back as
        // they are.
        let short = with_room(256, 64, |_| -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(libc::ENOMEM))
        });
        assert_eq!(short.unwrap_err().raw_os_error(), Some(libc::ENOMEM));
        let mut calls = 0;
        let denied = with_room(256, 64, |_| -> std::io::Result<()> {
            calls += 1;
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        });
        assert_eq!(
            (denied.unwrap_err().raw_os_error(), calls),
            (Some(libc::EPERM), 1)
        );
    }

    /// This machine's answer, as the process takes it: with no backend named,
    /// its shards run rings exactly when io_uring is usable here, the ring a
    /// shard runs included.
    #[cfg(all(target_os = "linux", feature = "iouring"))]
    #[test]
    fn the_probe_decides_auto_on_this_machine() {
        let available = super::usable().is_ok();
        assert_eq!(is_ring(&decide(None, None, super::usable)), available);
        let named = std::env::var("NUCLEI_BACKEND").unwrap_or_default();
        if (named.is_empty() || named == "auto")
            && crate::proactor::Proactor::get().config().backend.is_none()
        {
            assert_eq!(
                is_ring(&super::chosen().map_err(|e| e.to_string())),
                available
            );
        }
    }
}
