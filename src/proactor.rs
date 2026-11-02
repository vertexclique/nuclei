//! The proactor facade of earlier nuclei versions, over the runtime's
//! shards: every shard drives its own IO, and these calls reach the shard
//! of the calling thread.

use core::future::Future;
use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::config::NucleiConfig;
use crate::rt::context;
use crate::rt::shard::Shared;

/// Which kernel interface nuclei's IO runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IoBackend {
    /// kqueue (macOS and the BSDs).
    Kqueue,
    /// epoll (Linux).
    Epoll,
    /// io_uring (Linux).
    IoUring,
    /// I/O completion ports (Windows).
    Iocp,
}

/// The process's proactor: its configuration, and the IO of the calling
/// thread's shard.
#[derive(Debug)]
pub struct Proactor {
    config: NucleiConfig,
}

static PROACTOR: OnceLock<Proactor> = OnceLock::new();

/// The shards of threads inside [`Proactor::wait`], by address.
type Waiting = kovan_map::HashMap<usize, Arc<Shared>>;

static WAITING: OnceLock<Waiting> = OnceLock::new();

fn waiting() -> &'static Waiting {
    // 64 is the smallest table the map accepts.
    WAITING.get_or_init(|| Waiting::with_capacity(64))
}

/// Ends the waits of every shard in `waiting`: a wake is left on each
/// one's poller, so a wait that has not begun yet (its thread is inside
/// `Proactor::wait`, before the poller) ends as soon as it does.
fn wake_all(waiting: &Waiting) {
    for (_, shard) in waiting.iter() {
        shard.notify();
    }
}

/// A thread's shard in [`WAITING`] while its `Proactor::wait` lasts: the
/// outermost wait on a shard enters it and takes it out, a wait nested in
/// it (a task the turn runs waiting too) finds it there and leaves it.
struct InWait(Option<usize>);

impl InWait {
    fn new(shard: &Arc<Shared>) -> Self {
        let key = Arc::as_ptr(shard) as usize;
        let entered = waiting().insert_if_absent(key, Arc::clone(shard)).is_none();
        Self(entered.then_some(key))
    }
}

impl Drop for InWait {
    fn drop(&mut self) {
        if let Some(key) = self.0 {
            waiting().remove(&key);
        }
    }
}

impl Proactor {
    /// The proactor, with the default configuration unless
    /// [`with_config`](Self::with_config) set one first.
    pub fn get() -> &'static Proactor {
        PROACTOR.get_or_init(|| Proactor {
            config: NucleiConfig::default(),
        })
    }

    /// Sets the proactor's configuration, and returns the proactor. It is
    /// the default every runtime starts from;
    /// [`Builder::io_uring`](crate::Builder::io_uring) gives one runtime
    /// ring settings of its own, checked when it is built.
    ///
    /// # Panics
    ///
    /// When the proactor was configured already, or used (which configures
    /// it with the defaults): a configuration is set once, before any IO.
    /// The signature returns the proactor itself, so it has no error to
    /// answer; `Builder::io_uring` answers its error instead.
    pub fn with_config(config: NucleiConfig) -> &'static Proactor {
        let mut config = Some(config);
        let proactor = PROACTOR.get_or_init(|| Proactor {
            config: config.take().unwrap_or_default(),
        });
        assert!(
            config.is_none(),
            "the proactor is configured already: call `with_config` once, before any IO"
        );
        proactor
    }

    /// The configuration in place.
    pub fn config(&self) -> &NucleiConfig {
        &self.config
    }

    /// Ends every [`wait`](Self::wait) in progress at once.
    pub fn wake(&self) {
        wake_all(waiting());
    }

    /// Drives the calling thread's shard once: runs its ready tasks, waits
    /// for IO (up to `duration`; `None` until some comes or [`wake`], which
    /// ends a wait made inside a task or a `block_on` too), handling at most
    /// `max_event_size` events, and runs the tasks the IO woke. Each of the
    /// two runs of tasks takes at most the runtime's
    /// [`event_interval`](crate::Builder::event_interval) of them, so tasks
    /// that keep waking each other cannot hold the call. Returns how many IO
    /// events it handled.
    ///
    /// [`wake`]: Self::wake
    ///
    /// # Errors
    ///
    /// When the thread's shard cannot start, or its poller fails.
    pub fn wait(&self, max_event_size: usize, duration: Option<Duration>) -> io::Result<usize> {
        // Inside a task or a `block_on`: the turn is the running shard's, which a wake must
        // reach as it reaches a thread's own (proofs/tla/park/Park.tla `unregistered_wait`).
        if let Some(events) = context::current(|current| {
            current.map(|s| {
                let _waiting = InWait::new(&s.shared);
                s.turn(max_event_size, duration)
            })
        }) {
            return events;
        }
        let shard = context::own_shard()?;
        let _current = context::enter(&shard);
        #[cfg(port)]
        shard.take_port_back();
        let _waiting = InWait::new(&shard.shared);
        shard.turn(max_event_size, duration)
    }

    /// The backend the calling thread's shard runs its IO on; on a thread
    /// that runs none, the one of every runtime that names none (the
    /// platform's poller when the backend named cannot be had: the shards
    /// then fail with the reason).
    pub fn backend() -> IoBackend {
        match context::current(|shard| shard.map(|s| s.io_backend())) {
            Some(running) => running,
            None => crate::rt::backend::chosen().map_or(POLLER, |b| {
                if b.rings() {
                    IoBackend::IoUring
                } else {
                    POLLER
                }
            }),
        }
    }
}

/// The platform's readiness poller.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "illumos"))]
pub(crate) const POLLER: IoBackend = IoBackend::Epoll;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
pub(crate) const POLLER: IoBackend = IoBackend::Kqueue;
#[cfg(target_os = "windows")]
pub(crate) const POLLER: IoBackend = IoBackend::Iocp;

/// Runs `future` to its end on the calling thread, driving its shard's IO
/// meanwhile: [`block_on`](crate::runtime::block_on).
pub fn drive<T>(future: impl Future<Output = T>) -> T {
    crate::runtime::block_on(future)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{Waiting, wake_all};
    use crate::rt::context;

    /// A wake that comes after the thread went into the wait, but before
    /// its shard waits on the poller, still ends that wait.
    #[test]
    fn a_wake_before_the_poller_wait_ends_it() {
        let shard = context::own_shard().unwrap();
        let _current = context::enter(&shard);
        let waiting = Waiting::with_capacity(64);
        waiting.insert(0, Arc::clone(&shard.shared));
        wake_all(&waiting);
        let start = Instant::now();
        shard.turn(64, Some(Duration::from_secs(10))).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the wake was lost: the wait ran {:?}",
            start.elapsed()
        );
    }

    /// A wake ends a wait made inside a `block_on`, on a thread that runs a
    /// shard: the waker keeps waking until the wait returns, so a wake that
    /// came before the wait began is followed by one that ends it.
    #[test]
    fn a_wake_ends_a_wait_on_a_thread_that_runs_a_shard() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use super::Proactor;

        let done = Arc::new(AtomicBool::new(false));
        let waker = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut backoff = Duration::from_micros(50);
                while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
                    Proactor::get().wake();
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_millis(10));
                }
            })
        };
        let start = Instant::now();
        crate::block_on(async {
            Proactor::get()
                .wait(64, Some(Duration::from_secs(10)))
                .unwrap()
        });
        let took = start.elapsed();
        done.store(true, Ordering::SeqCst);
        waker.join().unwrap();
        assert!(
            took < Duration::from_secs(5),
            "the wake did not end the wait: it ran {took:?}"
        );
    }
}
