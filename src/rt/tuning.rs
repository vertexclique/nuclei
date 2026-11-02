//! How a worker shard runs its loop ([`Tuning`]: the builder's scheduler
//! knobs) and the functions a runtime calls on its threads ([`ThreadHooks`]):
//! when one starts and stops (workers and blocking-pool threads), and when a
//! worker parks and wakes.

use std::sync::Arc;

/// Task runs between reads of the coarse clock; a power of two, so the run
/// loop tells the runs apart with a mask.
pub(crate) const QUARTZ_EVERY: u32 = 16;
/// Nanoseconds of task running after which a worker shard looks at its driver
/// (1 ms), whatever the count of runs.
pub(crate) const DRIVE_BOUND: u64 = 1_000_000;

/// How a worker shard runs its loop, and the hooks it calls when it parks;
/// a shard that is not a worker keeps the defaults.
#[derive(Clone, Debug)]
pub(crate) struct Tuning {
    /// Task runs between looks at the driver (timers and IO).
    pub(crate) driver_interval: u32,
    /// Nanoseconds of task running after which the driver gets a look, however
    /// few runs that was (read from [`Quartz`](super::quartz::Quartz) every
    /// [`QUARTZ_EVERY`] runs).
    pub(crate) drive_bound: u64,
    /// Task runs between looks at the inbox first.
    pub(crate) inbox_interval: u32,
    /// The most IO events one look at the driver takes.
    pub(crate) max_io_events: usize,
    /// A task woken by the running task runs next.
    pub(crate) lifo: bool,
    pub(crate) hooks: ThreadHooks,
    /// Jobs in flight on the runtime's blocking pool: while any is, a shard that
    /// runs dry parks, and a park one of them cuts short does not make it spin.
    pub(crate) blocking: Option<Arc<crate::sync::internal::AtomicUsize>>,
    /// The least a zero-copy write must have left for a send of it to go
    /// zero-copy on the worker's ring.
    #[cfg(ring)]
    pub(crate) zero_copy_min: usize,
    /// The shard times its IO operations, by kind (`Builder::record_latencies`).
    pub(crate) record_latencies: bool,
    /// The shard counts its IO operations (`Builder::count_io`).
    #[cfg(feature = "io-stats")]
    pub(crate) count_io: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            driver_interval: super::shard::DRIVER_INTERVAL,
            drive_bound: DRIVE_BOUND,
            inbox_interval: super::shard::INBOX_INTERVAL,
            max_io_events: usize::MAX,
            lifo: true,
            hooks: ThreadHooks::default(),
            blocking: None,
            #[cfg(ring)]
            zero_copy_min: crate::io::source::ring::SEND_ZC_MIN,
            record_latencies: false,
            #[cfg(feature = "io-stats")]
            count_io: false,
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
            call_caught(start);
        }
        Stopping(self.stop.clone())
    }

    /// Calls the park hook: the worker is about to sleep.
    #[inline(always)]
    pub(crate) fn parked(&self) {
        if let Some(park) = &self.park {
            call_caught(park);
        }
    }

    /// Calls the unpark hook: the worker woke from its sleep.
    #[inline(always)]
    pub(crate) fn unparked(&self) {
        if let Some(unpark) = &self.unpark {
            call_caught(unpark);
        }
    }
}

/// Calls a thread hook; one that panics is let go of once the panic hook
/// reported it, and the thread goes on. The thread holds what the runtime
/// cannot lose (a shard and the tasks it queues, a pool thread's count, a
/// park under way), and nothing waits for the hook to hand its panic to
/// (proofs/tla/hooks, proofs/tla/pool/Start.tla).
#[cold]
#[inline(never)]
fn call_caught(hook: &Hook) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook()));
}

/// Calls a thread's stop hook when it drops.
pub(crate) struct Stopping(Option<Hook>);

impl Drop for Stopping {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            call_caught(stop);
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

/// What a task hook is told about the task.
#[derive(Debug)]
pub struct TaskMeta<'a> {
    id: crate::task::Id,
    name: Option<Arc<str>>,
    _task: core::marker::PhantomData<&'a ()>,
}

impl TaskMeta<'_> {
    pub(crate) fn new(id: crate::task::Id) -> Self {
        Self {
            id,
            name: super::task::names::get(id),
            _task: core::marker::PhantomData,
        }
    }

    /// The task's id.
    pub fn id(&self) -> crate::task::Id {
        self.id
    }

    /// The task's name, as [`task::Builder::name`](crate::task::Builder::name)
    /// gave it; `None` for a task spawned without one.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// A task hook.
pub(crate) type TaskHook = Arc<dyn Fn(&TaskMeta<'_>) + Send + Sync>;

/// A runtime's task hooks, and the runtime to shut down when a task panics
/// if it does that; a task of a runtime with none carries none.
#[derive(Clone, Default)]
pub(crate) struct TaskHooks {
    pub(crate) spawn: Option<TaskHook>,
    pub(crate) before_poll: Option<TaskHook>,
    pub(crate) after_poll: Option<TaskHook>,
    pub(crate) terminate: Option<TaskHook>,
    /// Weak: the runtime holds its tasks' hooks.
    pub(crate) shutdown: Option<std::sync::Weak<super::runtime::Inner>>,
}

impl TaskHooks {
    pub(crate) fn any(&self) -> bool {
        self.spawn.is_some()
            || self.before_poll.is_some()
            || self.after_poll.is_some()
            || self.terminate.is_some()
            || self.shutdown.is_some()
    }
}

/// Whether any runtime of the process has task hooks: while none does, a
/// spawn that does not know its runtime looks for none.
pub(crate) static ANY_TASK_HOOKS: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// What a runtime does when one of its tasks panics
/// ([`Builder::unhandled_panic`](crate::runtime::Builder::unhandled_panic)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnhandledPanic {
    /// The panic is the task's outcome, for whoever awaits it; the runtime
    /// runs on.
    #[default]
    Ignore,
    /// The runtime shuts down at once: its workers stop and drop their
    /// tasks, and every `block_on` of it panics, those running and every
    /// later one. The panicked task's handle still reports the panic.
    ShutdownRuntime,
}

/// Calls `hook`, if set, for the task `id`. A hook that panics aborts the
/// process: it runs in the middle of a task's change of state.
pub(crate) fn call(hook: Option<&TaskHook>, id: crate::task::Id) {
    if let Some(hook) = hook {
        crate::rt::task::abort_on_panic(|| hook(&TaskMeta::new(id)));
    }
}
