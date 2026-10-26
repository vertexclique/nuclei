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

/// What a task hook is told about the task.
#[derive(Debug)]
pub struct TaskMeta<'a> {
    id: crate::task::Id,
    _task: core::marker::PhantomData<&'a ()>,
}

impl TaskMeta<'_> {
    pub(crate) fn new(id: crate::task::Id) -> Self {
        Self {
            id,
            _task: core::marker::PhantomData,
        }
    }

    /// The task's id.
    pub fn id(&self) -> crate::task::Id {
        self.id
    }
}

/// A task hook.
pub(crate) type TaskHook = Arc<dyn Fn(&TaskMeta<'_>) + Send + Sync>;

/// A runtime's task hooks; a task of a runtime with none carries none.
#[derive(Clone, Default)]
pub(crate) struct TaskHooks {
    pub(crate) spawn: Option<TaskHook>,
    pub(crate) before_poll: Option<TaskHook>,
    pub(crate) after_poll: Option<TaskHook>,
    pub(crate) terminate: Option<TaskHook>,
}

impl TaskHooks {
    pub(crate) fn any(&self) -> bool {
        self.spawn.is_some()
            || self.before_poll.is_some()
            || self.after_poll.is_some()
            || self.terminate.is_some()
    }
}

/// Whether any runtime of the process has task hooks: while none does, a
/// spawn that does not know its runtime looks for none.
pub(crate) static ANY_TASK_HOOKS: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Calls `hook`, if set, for the task `id`. A hook that panics aborts the
/// process: it runs in the middle of a task's change of state.
pub(crate) fn call(hook: Option<&TaskHook>, id: crate::task::Id) {
    if let Some(hook) = hook {
        crate::rt::task::abort_on_panic(|| hook(&TaskMeta::new(id)));
    }
}
