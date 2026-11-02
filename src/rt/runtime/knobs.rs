//! The builder's hooks and scheduler knobs: what the runtime calls on its
//! threads and around its tasks, how its workers run their loops, what it
//! does when a task panics, and the seed of its random choices.

use std::sync::Arc;

use super::{Builder, RngSeed, TaskMeta, UnhandledPanic};

impl Builder {
    /// Called on each thread the runtime starts (workers and blocking-pool
    /// threads), on that thread, before it does anything else. A panic in it
    /// is caught (after the panic hook reported it) and the thread goes on,
    /// as it does for every thread hook: the thread still holds what it was
    /// started for.
    pub fn on_thread_start(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.tuning.hooks.start = Some(Arc::new(f));
        self
    }

    /// Called on each thread the runtime starts, on that thread, as it ends
    /// (after a panic too). A panic in it is caught.
    pub fn on_thread_stop(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.tuning.hooks.stop = Some(Arc::new(f));
        self
    }

    /// Called on a worker thread just before it sleeps with nothing to run.
    /// A panic in it is caught, and the worker sleeps all the same.
    pub fn on_thread_park(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.tuning.hooks.park = Some(Arc::new(f));
        self
    }

    /// Called on a worker thread just after it wakes from a sleep. A panic
    /// in it is caught, and the worker runs on.
    pub fn on_thread_unpark(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.tuning.hooks.unpark = Some(Arc::new(f));
        self
    }

    /// Called with each task this runtime spawns (not the blocking pool's
    /// jobs), on the spawning thread, once the task exists. A panic in it
    /// unwinds out of the spawn, and the task is cancelled before it runs.
    pub fn on_task_spawn(mut self, f: impl Fn(&TaskMeta<'_>) + Send + Sync + 'static) -> Self {
        self.task_hooks.spawn = Some(Arc::new(f));
        self
    }

    /// Called just before each poll of a task of this runtime, on the thread
    /// that polls it. A panic in it is the poll's: the task completes with
    /// it, unpolled, for its handle.
    pub fn on_before_task_poll(
        mut self,
        f: impl Fn(&TaskMeta<'_>) + Send + Sync + 'static,
    ) -> Self {
        self.task_hooks.before_poll = Some(Arc::new(f));
        self
    }

    /// Called just after each poll of a task of this runtime, on the thread
    /// that polled it. A panic in it completes the task with that panic, for
    /// its handle (a value the poll gave is dropped), unless the poll
    /// panicked first: that panic stays the task's, and the hook's is logged
    /// at error.
    pub fn on_after_task_poll(mut self, f: impl Fn(&TaskMeta<'_>) + Send + Sync + 'static) -> Self {
        self.task_hooks.after_poll = Some(Arc::new(f));
        self
    }

    /// Called when a task of this runtime is done with: its future completed
    /// or was dropped by a cancel, on the thread that dropped it. A panic in
    /// it goes as a panic of the future's drop does: to the task's handle as
    /// its outcome, or, with no handle, to the log at error.
    pub fn on_task_terminate(mut self, f: impl Fn(&TaskMeta<'_>) + Send + Sync + 'static) -> Self {
        self.task_hooks.terminate = Some(Arc::new(f));
        self
    }

    /// Called with each poll of a task of this runtime that its worker's
    /// watch sees run past [`handoff_after`](Builder::handoff_after), on the
    /// watch's own thread: the task's id and name say which one holds its
    /// worker up. The task is the innermost one whose poll was running when
    /// the watch looked; the poll may have ended by the time the hook runs.
    /// A poll is told once, unless a `block_on` inside it ran a long poll of
    /// its own and the shard stayed on that thread: the outer poll may then
    /// be told again as it goes on. The shard is handed on first, so a slow
    /// hook delays no rescue, but every other worker waits for its next look
    /// while the hook runs: keep it short. A panic in it is caught (after the
    /// panic hook reported it) and the watch goes on. Only workers are
    /// watched; with `handoff_after(None)` [`build`](Builder::build) fails
    /// with `InvalidInput`.
    pub fn on_long_poll(mut self, f: impl Fn(&TaskMeta<'_>) + Send + Sync + 'static) -> Self {
        self.long_poll = Some(Arc::new(f));
        self
    }

    /// Task runs between a worker's looks at its timers and IO when it is
    /// busy (default 61): lower answers IO sooner, higher runs tasks with
    /// fewer interruptions. At least 1; [`build`](Builder::build) fails with
    /// `InvalidInput` at 0.
    pub fn event_interval(mut self, runs: u32) -> Self {
        self.tuning.driver_interval = runs;
        self
    }

    /// Task runs between the times a worker takes from its inbox (tasks
    /// woken by other threads) before its own queue (default 31). At least 1;
    /// [`build`](Builder::build) fails with `InvalidInput` at 0.
    pub fn global_queue_interval(mut self, runs: u32) -> Self {
        self.tuning.inbox_interval = runs;
        self
    }

    /// The most IO events a worker takes from its driver in one look
    /// (default: all that are ready). At least 1; [`build`](Builder::build)
    /// fails with `InvalidInput` at 0.
    pub fn max_io_events_per_tick(mut self, events: usize) -> Self {
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
