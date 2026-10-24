//! The executor API of earlier nuclei versions: [`init`], the
//! [`GlobalExecutorConfig`] builder, and growing or shrinking the default
//! runtime's workers. Each maps onto the process default
//! [`Runtime`](crate::Runtime).

use std::fmt;
use std::io;

use crate::rt::runtime::{self, Builder};
use crate::rt::task::Task;
use crate::rt::workers::Workers;

/// How the process default runtime is set up by [`init_with_config`].
#[derive(Default)]
pub struct GlobalExecutorConfig {
    env_var: Option<&'static str>,
    min_threads: Option<usize>,
    max_threads: Option<usize>,
    thread_name_fn: Option<Box<dyn Fn() -> String + Send + Sync>>,
}

impl fmt::Debug for GlobalExecutorConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlobalExecutorConfig")
            .field("env_var", &self.env_var)
            .field("min_threads", &self.min_threads)
            .field("max_threads", &self.max_threads)
            .finish_non_exhaustive()
    }
}

impl GlobalExecutorConfig {
    /// Reads the number of workers to start from the environment variable
    /// `env_var`, before [`with_min_threads`](Self::with_min_threads).
    pub fn with_env_var(mut self, env_var: &'static str) -> Self {
        self.env_var = Some(env_var);
        self
    }

    /// Starts this many workers (default: one per core), the fewest
    /// [`stop_thread`] leaves running.
    pub fn with_min_threads(mut self, min_threads: usize) -> Self {
        self.min_threads = Some(min_threads);
        self
    }

    /// The most workers the runtime may run, [`spawn_more_threads`]
    /// included (default: four times the minimum). Zero runs every task on
    /// the thread inside `block_on`, with no worker at all.
    pub fn with_max_threads(mut self, max_threads: usize) -> Self {
        self.max_threads = Some(max_threads);
        self
    }

    /// Names each worker thread.
    pub fn with_thread_name_fn(
        mut self,
        thread_name_fn: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        self.thread_name_fn = Some(Box::new(thread_name_fn));
        self
    }

    /// The runtime builder this configuration describes.
    fn builder(self) -> Builder {
        let from_env = self
            .env_var
            .and_then(|name| std::env::var(name).ok())
            .and_then(|threads| threads.parse::<usize>().ok());
        let min = from_env
            .or(self.min_threads)
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
        let mut builder = match self.max_threads {
            Some(0) => Builder::new().worker_threads(0),
            max => {
                let min = min.max(1);
                Builder::new()
                    .worker_threads(min)
                    .max_worker_threads(max.unwrap_or(min.saturating_mul(4)).max(min))
            }
        };
        if let Some(name) = self.thread_name_fn {
            builder = builder.thread_name_fn(name);
        }
        builder
    }
}

/// Starts the process default runtime now, with every setting at its
/// default, unless it is running already.
pub fn init() {
    runtime::default_runtime();
}

/// Starts the process default runtime now, as `config` says, unless it is
/// running already (then `config` is not used).
pub fn init_with_config(config: GlobalExecutorConfig) {
    let _ = config.builder().build_global();
}

/// Starts up to `count` more workers on the runtime this thread is inside
/// (or the default one), within its maximum; returns how many started.
///
/// # Errors
///
/// When a worker's poller cannot be made or its thread cannot start.
pub async fn spawn_more_threads(count: usize) -> io::Result<usize> {
    Workers::start(&runtime::current(), count)
}

/// Stops one worker of the runtime this thread is inside (or the default
/// one), unless only the runtime's minimum runs: it takes no new task from
/// now on and exits once the tasks it has are done. The task completes with
/// whether a worker was stopped, once it takes no new task.
pub fn stop_thread() -> Task<bool> {
    crate::runtime::spawn(async { runtime::current().workers.stop_one() })
}

/// Stops the worker this task runs on, as [`stop_thread`] stops one: false
/// on a thread that is not a worker of its runtime, or when only the
/// minimum runs.
pub fn stop_current_thread() -> Task<bool> {
    crate::runtime::spawn_local(async {
        let inner = runtime::current();
        crate::rt::context::current(|current| {
            current
                .filter(|shard| shard.worker_index(inner.id()).is_some())
                .is_some_and(|shard| inner.workers.stop(&shard.shared))
        })
    })
}
