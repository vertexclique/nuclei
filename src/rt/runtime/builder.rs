//! The [`Builder`]: every setting of a runtime, and [`build`](Builder::build),
//! which starts the runtime they describe.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use super::super::blocking::{Pool, PoolConfig};
use super::super::context;
use super::super::shard::Limits;
use super::super::workers::{Start, Workers};
use super::{
    DEFAULT, Inner, RngSeed, Runtime, RuntimeHandle, UnhandledPanic, allowed_cores, current_mask,
};

/// Builds a [`Runtime`]. Every setting is optional: an unset one takes the
/// default, which needs no configuration to work well.
///
/// ```
/// let runtime = nuclei::Builder::new().worker_threads(2).build().unwrap();
/// assert_eq!(runtime.block_on(async { 1 + 1 }), 2);
/// ```
#[must_use]
pub struct Builder {
    pub(super) worker_threads: Option<usize>,
    pub(super) thread_name: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    pub(super) thread_stack_size: Option<usize>,
    pub(super) max_nesting: Option<usize>,
    pub(super) max_blocking_threads: Option<usize>,
    pub(super) blocking_keep_alive: Option<Duration>,
    pub(super) pin_workers: Option<bool>,
    pub(super) local_spawn_margin: Option<usize>,
    pub(super) io_buffer_cap: Option<usize>,
    pub(super) max_worker_threads: Option<usize>,
    pub(super) backend: Option<crate::IoBackend>,
    pub(super) handoff_after: Option<Option<Duration>>,
    pub(super) start_paused: bool,
    pub(super) tuning: super::super::tuning::Tuning,
    pub(super) task_hooks: super::super::tuning::TaskHooks,
    pub(super) unhandled_panic: UnhandledPanic,
    pub(super) rng_seed: Option<RngSeed>,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

/// Worker thread stacks: deep enough for nested `block_on`.
const DEFAULT_STACK: usize = 8 * 1024 * 1024;
const DEFAULT_BLOCKING_THREADS: usize = 512;
const DEFAULT_KEEP_ALIVE: Duration = Duration::from_secs(10);
/// See [`Builder::local_spawn_margin`].
const DEFAULT_LOCAL_MARGIN: usize = 32;
/// See [`Builder::handoff_after`].
const DEFAULT_HANDOFF: Duration = Duration::from_millis(10);

/// The smallest worker or blocking thread stack. A worker starting, a timer
/// and a TCP connect take 77 KiB in a debug build and 64 KiB in a release
/// one (the shard is built on the stack before it moves to the heap), on
/// glibc 2.39 and 2.44 alike; this leaves room over the deepest of them.
const MIN_STACK: usize = 128 * 1024;
/// The shortest `handoff_after`: the watch measures in milliseconds.
const MIN_HANDOFF: Duration = Duration::from_millis(1);
/// The smallest table the guest map accepts; it grows past it on demand.
const GUESTS_CAPACITY: usize = 64;

impl Builder {
    /// Refuses a size no runtime can work with, as the configuration's mistake.
    fn validate(&self) -> io::Result<()> {
        use crate::config::refuse;
        if let Some(bytes) = self.thread_stack_size.filter(|&b| b < MIN_STACK) {
            return Err(refuse(
                "thread_stack_size",
                bytes,
                format_args!("at least {MIN_STACK} bytes"),
            ));
        }
        if let Some(depth) = self.max_nesting.filter(|&d| d < 1) {
            return Err(refuse("max_nesting", depth, "at least 1"));
        }
        if let Some(n) = self.max_blocking_threads.filter(|&n| n < 1) {
            return Err(refuse("max_blocking_threads", n, "at least 1"));
        }
        if let Some(Some(after)) = self
            .handoff_after
            .filter(|a| a.is_some_and(|a| a < MIN_HANDOFF))
        {
            return Err(refuse(
                "handoff_after",
                format_args!("{after:?}"),
                format_args!("at least {MIN_HANDOFF:?}, or None"),
            ));
        }
        Ok(())
    }

    /// A builder with every setting at its default.
    pub fn new() -> Self {
        Self {
            worker_threads: None,
            thread_name: None,
            thread_stack_size: None,
            max_nesting: None,
            max_blocking_threads: None,
            blocking_keep_alive: None,
            pin_workers: None,
            local_spawn_margin: None,
            io_buffer_cap: None,
            max_worker_threads: None,
            backend: None,
            handoff_after: None,
            start_paused: false,
            tuning: super::super::tuning::Tuning::default(),
            task_hooks: super::super::tuning::TaskHooks::default(),
            unhandled_panic: UnhandledPanic::Ignore,
            rng_seed: None,
        }
    }

    /// Worker threads, each running one shard. The default is one per core
    /// the process may run on. Zero runs every task on the thread that
    /// spawns it, while that thread is inside `block_on`.
    pub fn worker_threads(mut self, n: usize) -> Self {
        self.worker_threads = Some(n);
        self
    }

    /// The name of every thread the runtime starts (default `nuclei-worker`
    /// and `nuclei-blocking`).
    pub fn thread_name(mut self, name: impl Into<String>) -> Self {
        let name: String = name.into();
        self.thread_name = Some(Arc::new(move || name.clone()));
        self
    }

    /// A function naming each thread the runtime starts.
    pub fn thread_name_fn(mut self, f: impl Fn() -> String + Send + Sync + 'static) -> Self {
        self.thread_name = Some(Arc::new(f));
        self
    }

    /// The stack size of every thread the runtime starts, in bytes (default
    /// 8 MiB). At least 128 KiB; [`build`](Self::build) fails with `InvalidInput`
    /// below.
    pub fn thread_stack_size(mut self, bytes: usize) -> Self {
        self.thread_stack_size = Some(bytes);
        self
    }

    /// How deep `block_on` may nest on one thread (default 256). A call one
    /// level deeper panics instead of running the thread out of stack. At least
    /// 1; [`build`](Self::build) fails with `InvalidInput` below.
    pub fn max_nesting(mut self, depth: usize) -> Self {
        self.max_nesting = Some(depth);
        self
    }

    /// The most threads the blocking pool runs at once (default 512). At least 1;
    /// [`build`](Self::build) fails with `InvalidInput` below.
    pub fn max_blocking_threads(mut self, n: usize) -> Self {
        self.max_blocking_threads = Some(n);
        self
    }

    /// How long an idle blocking thread waits for work before it exits
    /// (default 10 seconds).
    pub fn thread_keep_alive(mut self, keep_alive: Duration) -> Self {
        self.blocking_keep_alive = Some(keep_alive);
        self
    }

    /// How many more live tasks than another shard a spawning thread's shard
    /// may hold and still take a task it spawns (default 32). A task spawned
    /// on a worker stays on it (so a task that waits for what it spawned
    /// keeps it on its own thread), and a task spawned on any other thread
    /// goes to that thread's home shard (where its spawns went last), unless
    /// that shard holds that many more tasks than a shard picked at random;
    /// then it goes to the other, which becomes the home. Zero balances
    /// strictly; a larger margin keeps more work together.
    pub fn local_spawn_margin(mut self, margin: usize) -> Self {
        self.local_spawn_margin = Some(margin);
        self
    }

    /// The most worker threads the runtime may start over its life, the
    /// ones it starts with included (default four times
    /// [`worker_threads`](Self::worker_threads)): what `spawn_more_threads`
    /// may add up to. A stopped worker's place is not taken again. A value
    /// below the worker threads is raised to them, whose count may come from
    /// the machine.
    pub fn max_worker_threads(mut self, n: usize) -> Self {
        self.max_worker_threads = Some(n);
        self
    }

    /// The most bytes each worker's IO buffers hold (default 64 MiB). Reads
    /// that had to wait land in these buffers, and writes that had to wait
    /// are copied into them; at the cap, IO waits for buffers to come back,
    /// so a worker's IO memory never grows past it. On io_uring the worker's
    /// receive buffers count against the cap too
    /// ([`recv_buffers`](crate::config::IoUringConfiguration::recv_buffers) of
    /// [`recv_buffer_size`](crate::config::IoUringConfiguration::recv_buffer_size)
    /// bytes, 4 MiB by default, with their descriptors), and the rest is
    /// left for the others. A cap that leaves less than 512 bytes beside the
    /// receive buffers is refused with `InvalidInput` when the worker starts;
    /// otherwise any value is accepted, and a request that cannot fit under a
    /// cap too small for it fails with an error that names this setting.
    pub fn io_buffer_cap(mut self, bytes: usize) -> Self {
        self.io_buffer_cap = Some(bytes);
        self
    }

    /// The kernel interface the workers run their IO on. Unset, it is the
    /// process's: the `backend` of
    /// [`NucleiConfig`](crate::config::NucleiConfig), or io_uring where the
    /// kernel lets this process have it and the platform's poller otherwise.
    /// A backend this process cannot have makes [`build`](Self::build) fail
    /// with the reason. A non-empty `NUCLEI_BACKEND` overrides it.
    pub fn backend(mut self, backend: crate::IoBackend) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Whether the runtime's clock starts paused (default false): see
    /// [`time::pause`](crate::time::pause). Its tasks then see time move only
    /// as they [`advance`](crate::time::advance) it, or as it jumps to the
    /// next timer whenever every shard of the runtime is idle.
    pub fn start_paused(mut self, paused: bool) -> Self {
        self.start_paused = paused;
        self
    }

    /// How long one poll of a task may run on a worker before the worker's
    /// other tasks go on without it (default 10 ms; `None`: never).
    ///
    /// A task that blocks inside its poll (a blocking call, a long loop
    /// without an `.await`) then holds up only itself: its worker's shard,
    /// with every other task, timer and IO of it, goes to another thread on
    /// the same core, and the blocked task carries on once its poll returns.
    /// At least 1 ms; [`build`](Self::build) fails with `InvalidInput` below.
    /// Tasks that `spawn_local` keeps on the blocked thread wait for it. The
    /// watch costs each poll two plain stores and a load; with it on, an
    /// io_uring worker's ring takes submissions from any thread.
    pub fn handoff_after(mut self, after: Option<Duration>) -> Self {
        self.handoff_after = Some(after);
        self
    }

    /// Whether each worker thread is pinned to its own core (Linux; default
    /// true). A pinned shard keeps its cache warm; unpinned, the scheduler may
    /// move it.
    pub fn pin_workers(mut self, pin: bool) -> Self {
        self.pin_workers = Some(pin);
        self
    }

    /// Starts the runtime.
    ///
    /// # Errors
    ///
    /// When a worker's poller cannot be made or its thread cannot start.
    pub fn build(self) -> io::Result<Runtime> {
        let workers = self
            .worker_threads
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
        self.validate()?;
        let stack = self.thread_stack_size.unwrap_or(DEFAULT_STACK);
        let limits = Limits {
            max_nesting: self.max_nesting.unwrap_or(context::DEFAULT_MAX_NESTING),
            buffer_cap: self.io_buffer_cap.unwrap_or(crate::buf::DEFAULT_CAP),
            backend: super::super::backend::for_runtime(self.backend)?,
        };
        #[cfg(ring)]
        let rings = limits.backend.rings();
        let blocking_name = self
            .thread_name
            .clone()
            .unwrap_or_else(|| Arc::new(|| "nuclei-blocking".to_string()));
        let worker_name = self
            .thread_name
            .unwrap_or_else(|| Arc::new(|| "nuclei-worker".to_string()));
        let pool = Pool::new(PoolConfig {
            hooks: self.tuning.hooks.clone(),
            max_threads: self
                .max_blocking_threads
                .unwrap_or(DEFAULT_BLOCKING_THREADS)
                .max(1),
            keep_alive: self.blocking_keep_alive.unwrap_or(DEFAULT_KEEP_ALIVE),
            stack_size: self.thread_stack_size,
            name: blocking_name,
            cpu_mask: current_mask(),
        });
        static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let (exit_tx, exits) = kovan_channel::unbounded();
        let cores = if self.pin_workers.unwrap_or(true) {
            allowed_cores()
        } else {
            Vec::new()
        };
        let capacity = self
            .max_worker_threads
            .unwrap_or(workers.saturating_mul(4))
            .max(workers);
        let shutdown_on_panic = self.unhandled_panic == UnhandledPanic::ShutdownRuntime;
        let mut task_hooks = self.task_hooks;
        if task_hooks.before_poll.is_some() || task_hooks.after_poll.is_some() {
            super::super::tuning::ANY_POLL_HOOKS.store(true, std::sync::atomic::Ordering::Release);
        }
        let inner = Arc::new_cyclic(|this| {
            if shutdown_on_panic {
                task_hooks.shutdown = Some(this.clone());
            }
            Inner {
                id: IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                workers: Workers::new(
                    capacity,
                    workers,
                    Start {
                        limits,
                        tuning: super::super::tuning::Tuning {
                            blocking: Some(pool.jobs_counter()),
                            ..self.tuning.clone()
                        },
                        seed: self.rng_seed,
                        stack,
                        name: worker_name,
                        cores,
                        handoff_after: self.handoff_after.unwrap_or(Some(DEFAULT_HANDOFF)),
                    },
                    exit_tx,
                ),
                pool,
                local_margin: self.local_spawn_margin.unwrap_or(DEFAULT_LOCAL_MARGIN),
                #[cfg(ring)]
                rings,
                clock: crate::time::clock::Clock::new(self.start_paused),
                task_hooks: task_hooks.any().then(|| {
                    super::super::tuning::ANY_TASK_HOOKS
                        .store(true, std::sync::atomic::Ordering::Release);
                    Arc::new(task_hooks)
                }),
                shutdown_on_panic,
                panicked: std::sync::atomic::AtomicBool::new(false),
                on_panic: crate::sync::Notify::new(),
                rng_seed: self.rng_seed,
                guests: kovan_map::HashMap::with_capacity(GUESTS_CAPACITY),
                lingering: super::super::linger::Lingering::new(),
            }
        });
        inner.pool.set_owner(Arc::downgrade(&inner));
        let runtime = Runtime { inner, exits };
        // On an error the runtime drops: the workers already started stop.
        Workers::start(&runtime.inner, workers)?;
        Ok(runtime)
    }

    /// Starts the runtime and makes it the process default, the one the free
    /// functions ([`spawn`](crate::spawn), [`block_on`](crate::block_on),
    /// ...) use outside any other runtime.
    ///
    /// # Errors
    ///
    /// As for [`build`](Self::build), and when the default runtime is
    /// already running (it was built, or a free function started it).
    pub fn build_global(self) -> io::Result<RuntimeHandle> {
        let runtime = self.build()?;
        let handle = runtime.handle();
        DEFAULT
            .set(runtime)
            .map_err(|_| io::Error::other("the default runtime is already running"))?;
        Ok(handle)
    }
}

impl fmt::Debug for Builder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Builder")
            .field("worker_threads", &self.worker_threads)
            .field("thread_stack_size", &self.thread_stack_size)
            .field("max_nesting", &self.max_nesting)
            .field("max_blocking_threads", &self.max_blocking_threads)
            .field("thread_keep_alive", &self.blocking_keep_alive)
            .field("pin_workers", &self.pin_workers)
            .field("local_spawn_margin", &self.local_spawn_margin)
            .field("io_buffer_cap", &self.io_buffer_cap)
            .field("start_paused", &self.start_paused)
            .finish_non_exhaustive()
    }
}
