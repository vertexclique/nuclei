//! Where a shard sleeps when it has nothing to run, and how another thread
//! wakes it.
//!
//! A [`Poller`] belongs to one shard thread: [`Poller::wait_at_most`] blocks until a
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
#[cfg(ring)]
pub(crate) mod ring;
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

#[cfg(windows)]
pub(crate) mod iocp;
#[cfg(windows)]
pub(crate) use self::iocp::{Notifier, Poller};

#[cfg(all(test, not(any(target_os = "linux", target_os = "android"))))]
thread_local! {
    /// This thread's next notifications a test makes fail, as a failing
    /// kernel call would.
    static FAIL_NOTIFIES: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

/// Makes this thread's next `n` notifications fail; returns how many were
/// still to fail.
#[cfg(all(test, not(any(target_os = "linux", target_os = "android"))))]
pub(crate) fn fail_notifies(n: u32) -> u32 {
    FAIL_NOTIFIES.replace(n)
}

/// Notifications that fail in a row before the failure is reported.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const NOTIFY_TRIES: u32 = 64;

/// Makes a notification's kernel call, `post`, until the kernel takes it.
/// The waker that took a shard's flag back is the only one that notifies it
/// (`ParkState::wake`): a failed call left at that would leave the shard
/// asleep with its work for good (proofs/tla/park/Park.tla
/// `notify_swallowed`). kqueue's `kevent` and the port's
/// `PostQueuedCompletionStatus` fail only for want of a resource, which a
/// try a moment later finds; after [`NOTIFY_TRIES`] in a row the failure is
/// reported.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn notify_until_taken(post: impl Fn() -> std::io::Result<()>) {
    let mut tries = 1;
    while let Err(err) = post_notification(&post) {
        if tries == NOTIFY_TRIES {
            log::error!(
                "a sleeping worker could not be woken ({err}): it runs its new work at its next timer or IO"
            );
            return;
        }
        tries += 1;
        std::thread::yield_now();
    }
}

/// A notification's kernel call, `post`, or the failure a test asked for
/// in its place (kqueue's and the port's: an eventfd's write cannot fail).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[inline(always)]
fn post_notification(post: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_NOTIFIES.with(|left| {
        let fails = left.get() > 0;
        left.set(left.get().saturating_sub(1));
        fails
    }) {
        return Err(std::io::ErrorKind::Interrupted.into());
    }
    post()
}

/// What a registered source became ready for. A hang-up or an error counts
/// as both: the operations waiting on it then see it.
#[cfg(readiness)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Readiness {
    pub(crate) readable: bool,
    pub(crate) writable: bool,
    /// Urgent (out-of-band) data came: a read stops at its mark.
    pub(crate) urgent: bool,
    /// The peer hung up, or the socket failed.
    pub(crate) hangup: bool,
}

#[cfg(all(test, readiness))]
mod tests;
