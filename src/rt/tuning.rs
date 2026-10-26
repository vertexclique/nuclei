//! How a worker shard runs its loop ([`Tuning`]: the builder's scheduler
//! knobs) and the functions a runtime calls on its threads ([`ThreadHooks`]):
//! when one starts and stops (workers and blocking-pool threads), and when a
//! worker parks and wakes.

use std::sync::Arc;

/// How a worker shard runs its loop, and the hooks it calls when it parks;
/// a shard that is not a worker keeps the defaults.
#[derive(Clone, Debug)]
pub(crate) struct Tuning {
    /// Task runs between looks at the driver (timers and IO).
    pub(crate) driver_interval: u32,
    /// Task runs between looks at the inbox first.
    pub(crate) inbox_interval: u32,
    /// The most IO events one look at the driver takes.
    pub(crate) max_io_events: usize,
    pub(crate) hooks: ThreadHooks,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            driver_interval: super::shard::DRIVER_INTERVAL,
            inbox_interval: super::shard::INBOX_INTERVAL,
            max_io_events: usize::MAX,
            hooks: ThreadHooks::default(),
        }
    }
}

/// A hook: called with nothing, on the thread it is about.
pub(crate) type Hook = Arc<dyn Fn() + Send + Sync>;

/// A runtime's thread hooks; each unset one costs nothing.
#[derive(Clone, Default)]
pub(crate) struct ThreadHooks {
    pub(crate) start: Option<Hook>,
    pub(crate) stop: Option<Hook>,
    pub(crate) park: Option<Hook>,
    pub(crate) unpark: Option<Hook>,
}

impl ThreadHooks {
    /// Calls the start hook now and the stop hook when the guard drops, even
    /// when the thread ends by a panic.
    pub(crate) fn started(&self) -> Stopping {
        if let Some(start) = &self.start {
            start();
        }
        Stopping(self.stop.clone())
    }
}

/// Calls a thread's stop hook when it drops.
pub(crate) struct Stopping(Option<Hook>);

impl Drop for Stopping {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop();
        }
    }
}

impl core::fmt::Debug for ThreadHooks {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ThreadHooks")
            .field("start", &self.start.is_some())
            .field("stop", &self.stop.is_some())
            .field("park", &self.park.is_some())
            .field("unpark", &self.unpark.is_some())
            .finish()
    }
}
