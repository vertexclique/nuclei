//! The proactor facade of earlier nuclei versions, over the runtime's
//! shards: every shard drives its own IO, and these calls reach the shard
//! of the calling thread.

use core::future::Future;
use std::io;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
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

/// The shards of the threads inside [`Proactor::wait`].
static WAITING: Mutex<Vec<Arc<Shared>>> = Mutex::new(Vec::new());

fn waiting() -> std::sync::MutexGuard<'static, Vec<Arc<Shared>>> {
    WAITING.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Proactor {
    /// The proactor, with the default configuration unless
    /// [`with_config`](Self::with_config) set one first.
    pub fn get() -> &'static Proactor {
        PROACTOR.get_or_init(|| Proactor {
            config: NucleiConfig::default(),
        })
    }

    /// Sets the proactor's configuration, and returns the proactor.
    ///
    /// # Panics
    ///
    /// When the proactor was configured already, or used (which configures
    /// it with the defaults): a configuration is set once, before any IO.
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
        for shard in waiting().iter() {
            shard.unpark();
        }
    }

    /// Drives the calling thread's shard once: runs its ready tasks, waits
    /// for IO (up to `duration`; `None` until some comes or [`wake`]),
    /// handling at most `max_event_size` events, and runs the tasks the IO
    /// woke. Returns how many IO events it handled.
    ///
    /// [`wake`]: Self::wake
    ///
    /// # Errors
    ///
    /// When the thread's shard cannot start, or its poller fails.
    pub fn wait(&self, max_event_size: usize, duration: Option<Duration>) -> io::Result<usize> {
        if let Some(events) =
            context::current(|current| current.map(|s| s.turn(max_event_size, duration)))
        {
            return events;
        }
        let shard = context::own_shard()?;
        let _current = context::enter(&shard);
        #[cfg(port)]
        shard.take_port_back();
        waiting().push(Arc::clone(&shard.shared));
        let events = shard.turn(max_event_size, duration);
        waiting().retain(|s| !Arc::ptr_eq(s, &shard.shared));
        events
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
