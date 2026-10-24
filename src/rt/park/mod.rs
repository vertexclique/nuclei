//! Where a shard sleeps when it has nothing to run, and how another thread
//! wakes it.
//!
//! A [`Poller`] belongs to one shard thread: [`Poller::wait`] blocks until a
//! [`Notifier`] fires or the timeout passes. A notifier is cheap to clone and
//! may fire from any thread, any number of times: wakes that land while the
//! shard runs collapse into one return of the next wait. The IO drivers wait
//! on the same kernel object, so a shard sleeping for IO wakes for a remote
//! task too.
//!
//! On Linux the poller is an epoll instance with an eventfd registered in it;
//! on macOS and the BSDs a kqueue with an `EVFILT_USER` event.

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) use self::linux::{Notifier, Poller, Registrar};

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
mod kqueue;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
pub(crate) use self::kqueue::{Notifier, Poller, Registrar};

/// What a registered source became ready for. A hang-up or an error counts
/// as both: the operations waiting on it then see it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Readiness {
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    /// Urgent (out-of-band) data came: a read stops at its mark.
    pub(crate) urgent: bool,
    /// The peer hung up, or the socket failed.
    pub(crate) hangup: bool,
}

#[cfg(test)]
mod tests;
