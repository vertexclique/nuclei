//! Which kernel interface a runtime's shards drive their IO with.
//!
//! A program names it with [`Builder::backend`](crate::Builder::backend)
//! for one runtime, or with the `backend` field of
//! [`NucleiConfig`](crate::config::NucleiConfig) for every runtime that
//! names none; `NUCLEI_BACKEND` (`auto`, `io_uring`, `epoll`, `kqueue`,
//! `iocp`), when set, overrides both. Named by nobody, it is `auto`:
//! io_uring when the kernel lets this process make the ring its shards use
//! (see `syscore::linux::probe`, which checks without risking the process
//! under a seccomp filter), and the platform's own poller otherwise (its
//! I/O completion port on Windows).
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
            Backend::Ring(cfg) => Backend::Ring(crate::syscore::linux::iouring::ring::RingConfig {
                multi_issuer: true,
                ..cfg
            }),
            #[cfg(ring)]
            Backend::RingOrPoll(cfg) => {
                Backend::RingOrPoll(crate::syscore::linux::iouring::ring::RingConfig {
                    multi_issuer: true,
                    ..cfg
                })
            }
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
}

/// The backend of a runtime that names `named`: the process's default if it
/// names none, or if `NUCLEI_BACKEND` overrides it.
pub(crate) fn for_runtime(named: Option<IoBackend>) -> io::Result<Backend> {
    match (env_override(), named) {
        (None, Some(named)) => decide(None, Some(named), usable).map_err(unsupported),
        _ => chosen(),
    }
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

/// Whether this process can use io_uring (the probe runs once), or why not.
#[cfg(ring)]
fn usable() -> Result<(), String> {
    use crate::syscore::linux::probe::{Probe, probe};
    static PROBED: OnceLock<Result<(), String>> = OnceLock::new();
    PROBED
        .get_or_init(|| match probe() {
            Probe::Available => Ok(()),
            Probe::Unavailable(why) => Err(why.to_string()),
        })
        .clone()
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

/// The ring the shards make, as the process's configuration says; `auto`:
/// whether `auto` chose it.
fn ring(auto: bool) -> Backend {
    #[cfg(ring)]
    {
        let cfg = (&crate::proactor::Proactor::get().config().iouring).into();
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
    /// its shards run rings exactly when io_uring is usable here.
    #[cfg(all(target_os = "linux", feature = "iouring"))]
    #[test]
    fn the_probe_decides_auto_on_this_machine() {
        use crate::syscore::linux::probe::{Probe, probe};
        let available = probe() == Probe::Available;
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
