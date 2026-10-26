//! The builder's hooks and scheduler knobs: what the runtime calls on its
//! threads and around its tasks, and how its workers run their loops.

use std::sync::Arc;

use super::{Builder, TaskMeta};

impl Builder {
    /// Called on each thread the runtime starts (workers and blocking-pool
    /// threads), on that thread, before it does anything else.
    pub fn on_thread_start(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.tuning.hooks.start = Some(Arc::new(f));
        self
    }

    /// Called on each thread the runtime starts, on that thread, as it ends
    /// (after a panic too).
    pub fn on_thread_stop(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.tuning.hooks.stop = Some(Arc::new(f));
        self
    }

    /// Called on a worker thread just before it sleeps with nothing to run.
    pub fn on_thread_park(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.tuning.hooks.park = Some(Arc::new(f));
        self
    }

    /// Called on a worker thread just after it wakes from a sleep.
    pub fn on_thread_unpark(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.tuning.hooks.unpark = Some(Arc::new(f));
        self
    }

    /// Called with each task this runtime spawns (not the blocking pool's
    /// jobs), on the spawning thread, once the task exists.
    pub fn on_task_spawn(mut self, f: impl Fn(&TaskMeta<'_>) + Send + Sync + 'static) -> Self {
        self.task_hooks.spawn = Some(Arc::new(f));
        self
    }

    /// Called just before each poll of a task of this runtime, on the thread
    /// that polls it.
    pub fn on_before_task_poll(
        mut self,
        f: impl Fn(&TaskMeta<'_>) + Send + Sync + 'static,
    ) -> Self {
        self.task_hooks.before_poll = Some(Arc::new(f));
        self
    }

    /// Called just after each poll of a task of this runtime, on the thread
    /// that polled it.
    pub fn on_after_task_poll(mut self, f: impl Fn(&TaskMeta<'_>) + Send + Sync + 'static) -> Self {
        self.task_hooks.after_poll = Some(Arc::new(f));
        self
    }

    /// Called when a task of this runtime is done with: its future completed
    /// or was dropped by a cancel, on the thread that dropped it.
    pub fn on_task_terminate(mut self, f: impl Fn(&TaskMeta<'_>) + Send + Sync + 'static) -> Self {
        self.task_hooks.terminate = Some(Arc::new(f));
        self
    }

    /// Task runs between a worker's looks at its timers and IO when it is
    /// busy (default 61): lower answers IO sooner, higher runs tasks with
    /// fewer interruptions.
    ///
    /// # Panics
    ///
    /// When `runs` is zero.
    pub fn event_interval(mut self, runs: u32) -> Self {
        assert!(runs > 0, "event_interval must be at least 1");
        self.tuning.driver_interval = runs;
        self
    }

    /// Task runs between the times a worker takes from its inbox (tasks
    /// woken by other threads) before its own queue (default 31).
    ///
    /// # Panics
    ///
    /// When `runs` is zero.
    pub fn global_queue_interval(mut self, runs: u32) -> Self {
        assert!(runs > 0, "global_queue_interval must be at least 1");
        self.tuning.inbox_interval = runs;
        self
    }

    /// The most IO events a worker takes from its driver in one look
    /// (default: all that are ready).
    ///
    /// # Panics
    ///
    /// When `events` is zero.
    pub fn max_io_events_per_tick(mut self, events: usize) -> Self {
        assert!(events > 0, "max_io_events_per_tick must be at least 1");
        self.tuning.max_io_events = events;
        self
    }
}
