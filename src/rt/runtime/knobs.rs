//! The builder's hooks and scheduler knobs: what the runtime calls on its
//! threads and around its tasks, how its workers run their loops, what it
//! does when a task panics, and the seed of its random choices.

use std::sync::Arc;

use super::{Builder, RngSeed, TaskMeta, UnhandledPanic};

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

    /// Turns off each worker's LIFO slot, where a task woken by the task
    /// running on the worker goes to run next. Without it the woken task
    /// goes to the back of the worker's queue: fairer when the queue is
    /// long, slower for a task that hands work to another and waits for it.
    pub fn disable_lifo_slot(mut self) -> Self {
        self.tuning.lifo = false;
        self
    }

    /// Seeds the runtime's random choices, so that they repeat run after
    /// run; see [`RngSeed`].
    pub fn rng_seed(mut self, seed: RngSeed) -> Self {
        self.rng_seed = Some(seed);
        self
    }

    /// What the runtime does when one of its tasks panics (default
    /// [`UnhandledPanic::Ignore`]: the panic goes to the task's handle).
    ///
    /// ```
    /// use nuclei::runtime::{Runtime, UnhandledPanic};
    ///
    /// let rt = Runtime::builder()
    ///     .unhandled_panic(UnhandledPanic::ShutdownRuntime)
    ///     .build()
    ///     .unwrap();
    /// let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    ///     rt.block_on(async {
    ///         let _task = nuclei::spawn(async { panic!("a task fails") });
    ///         core::future::pending::<()>().await
    ///     })
    /// }));
    /// assert!(ran.is_err(), "the block_on panics once the task did");
    /// ```
    pub fn unhandled_panic(mut self, policy: UnhandledPanic) -> Self {
        self.unhandled_panic = policy;
        self
    }
}
