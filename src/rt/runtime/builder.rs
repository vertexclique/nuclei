//! The [`Builder`]: every setting of a runtime, checked once in
//! [`build`](Builder::build), which starts the runtime they describe.
//!
//! A setter only records its value; `build` checks them all, each against
//! what a runtime can work with and against the others, and refuses the
//! first one that does not fit with `InvalidInput` naming it. No setter
//! panics.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use super::super::blocking::{Pool, PoolConfig};
use super::super::context;
use super::super::shard::Limits;
use super::super::workers::{Start, Workers};
use super::{DEFAULT, Inner, RngSeed, Runtime, RuntimeHandle, UnhandledPanic};

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
    pub(super) io_uring: Option<crate::config::IoUringConfiguration>,
    pub(super) worker_cores: Option<Vec<usize>>,
    pub(super) blocking_cores: Option<Vec<usize>>,
    pub(super) handoff_after: Option<Option<Duration>>,
    pub(super) start_paused: bool,
    pub(super) record_latencies: bool,
    #[cfg(feature = "io-stats")]
    pub(super) count_io: bool,
    pub(super) tuning: super::super::tuning::Tuning,
    pub(super) task_hooks: super::super::tuning::TaskHooks,
    pub(super) long_poll: Option<super::super::tuning::TaskHook>,
    pub(super) unhandled_panic: UnhandledPanic,
    pub(super) rng_seed: Option<RngSeed>,
    pub(super) io: super::io_knobs::IoSettings,
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
pub(super) const GUESTS_CAPACITY: usize = 64;

impl Builder {
    /// Refuses a setting no runtime can work with, or one that does not fit
    /// with another, as the configuration's mistake.
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
        if let (Some(workers), Some(max)) = (self.worker_threads, self.max_worker_threads)
            && max < workers
        {
            return Err(refuse(
                "max_worker_threads",
                max,
                format_args!("at least worker_threads ({workers})"),
            ));
        }
        let handoff = self.handoff_after.unwrap_or(Some(DEFAULT_HANDOFF));
        if let Some(after) = handoff.filter(|&a| a < MIN_HANDOFF) {
            return Err(refuse(
                "handoff_after",
                format_args!("{after:?}"),
                format_args!("at least {MIN_HANDOFF:?}, or None"),
            ));
        }
        if self.long_poll.is_some() && handoff.is_none() {
            return Err(refuse(
                "on_long_poll",
                "set with handoff_after(None)",
                "used with handoff_after set to a duration, which is what a long poll passes",
            ));
        }
        let tuning = &self.tuning;
        for (setting, runs) in [
            ("event_interval", tuning.driver_interval),
            ("global_queue_interval", tuning.inbox_interval),
        ] {
            if runs < 1 {
                return Err(refuse(setting, runs, "at least 1"));
            }
        }
        if tuning.max_io_events < 1 {
            return Err(refuse("max_io_events_per_tick", 0, "at least 1"));
        }
        if let Some(cfg) = &self.io_uring {
            cfg.validate()?;
        }
        super::cores::validate("worker_cores", self.worker_cores.as_deref())?;
        super::cores::validate("blocking_cores", self.blocking_cores.as_deref())?;
        self.io.validate()
    }

    /// The workers the runtime starts with: `worker_threads`, else one per
    /// core of `worker_cores`, else one per core the process may run on;
    /// an unset count is held to `max_worker_threads`.
    fn worker_count(&self) -> usize {
        if let Some(n) = self.worker_threads {
            return n;
        }
        let default = self.worker_cores.as_ref().map_or_else(
            || std::thread::available_parallelism().map_or(1, usize::from),
            Vec::len,
        );
        self.max_worker_threads
            .map_or(default, |max| default.min(max))
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
            io_uring: None,
            worker_cores: None,
            blocking_cores: None,
            handoff_after: None,
            start_paused: false,
            record_latencies: false,
            #[cfg(feature = "io-stats")]
            count_io: false,
            tuning: super::super::tuning::Tuning::default(),
            task_hooks: super::super::tuning::TaskHooks::default(),
            long_poll: None,
            unhandled_panic: UnhandledPanic::Ignore,
            rng_seed: None,
            io: super::io_knobs::IoSettings::default(),
        }
    }

    /// Worker threads, each running one shard. The default is one per core
    /// of [`worker_cores`](Self::worker_cores) when it is set, else one per
    /// core the process may run on, held to
    /// [`max_worker_threads`](Self::max_worker_threads) when that is set.
    /// Zero runs every task on the thread that spawns it, while that thread
    /// is inside `block_on`.
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
    /// ones it starts with included (default four times the workers it
    /// starts with): what `spawn_more_threads` may add up to. A stopped
    /// worker's place is not taken again. Below an explicit
    /// [`worker_threads`](Self::worker_threads), [`build`](Self::build) fails
    /// with `InvalidInput`; with `worker_threads` unset, the runtime starts
    /// with at most this many.
    pub fn max_worker_threads(mut self, n: usize) -> Self {
        self.max_worker_threads = Some(n);
        self
    }

    /// The most bytes each worker's IO buffers hold (default 64 MiB). Reads
    /// that had to wait land in these buffers, and writes that had to wait
    /// are copied into them; at the cap, IO waits for buffers to come back,
    /// so a worker's IO memory never grows past it. A thread that serves the
    /// runtime and runs no shard (one of its blocking pool, one inside it with
    /// [`enter`](crate::Runtime::enter), a worker whose shard was handed over)
    /// holds as much at most in a pool of its own (one that held more before
    /// `enter` takes no new memory until it is back under the cap). On io_uring the worker's
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

    /// The settings the workers' io_uring rings are made with, for this
    /// runtime alone (default: the process's, from
    /// [`Proactor::with_config`](crate::Proactor::with_config)). Checked in
    /// [`build`](Self::build), which fails with `InvalidInput` naming the
    /// setting a ring cannot work with, on every platform; only the workers
    /// of a runtime whose IO runs on io_uring make rings. Whether io_uring
    /// is chosen when no backend is named is decided with these settings.
    /// The shard of a thread inside `block_on` keeps the process's settings:
    /// it serves every runtime the thread enters.
    pub fn io_uring(mut self, config: crate::config::IoUringConfiguration) -> Self {
        self.io_uring = Some(config);
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
    /// watch costs each poll two plain stores and a load, and each call a
    /// task makes into its worker (a wake, a spawn, a send or a receive) two
    /// loads and two stores; with it on, an io_uring worker's ring takes
    /// submissions from any thread.
    pub fn handoff_after(mut self, after: Option<Duration>) -> Self {
        self.handoff_after = Some(after);
        self
    }

    /// Whether each worker thread is pinned to its own core (Linux and
    /// Windows; default true). A pinned shard keeps its cache warm; unpinned,
    /// the scheduler may move it. With
    /// [`worker_cores`](Self::worker_cores) set, each worker is pinned to its
    /// core of that list, or, unpinned, runs on any core of the list.
    pub fn pin_workers(mut self, pin: bool) -> Self {
        self.pin_workers = Some(pin);
        self
    }

    /// Counts how long each task poll runs, and (with the `io-stats` feature)
    /// how long each IO operation waits for the kernel, by kind, in log2
    /// histograms each worker's [`ShardStats`](crate::stats::ShardStats)
    /// carries (default false: no clock is read for it). The poll times cost
    /// two clock reads and a call around every poll, of every runtime of the
    /// process while one records them; the IO latencies two clock reads per
    /// operation of this runtime's workers.
    pub fn record_latencies(mut self, record: bool) -> Self {
        self.record_latencies = record;
        self
    }

    /// Counts each worker's IO operations (reads, writes, the bytes of each,
    /// accepts) in its [`ShardStats::io`](crate::stats::ShardStats::io)
    /// (default false: `io` is `None`). With the `io-stats` feature only,
    /// which costs every IO call a few instructions, counted or not: off, a
    /// count is one load of a flag per operation; on, a few plain
    /// thread-local adds per operation and a drain each time a shard
    /// publishes its counts, on every thread of the process while one
    /// runtime counts.
    #[cfg(feature = "io-stats")]
    pub fn count_io(mut self, count: bool) -> Self {
        self.count_io = count;
        self
    }

    /// Starts the runtime.
    ///
    /// # Errors
    ///
    /// `InvalidInput` naming a setting that no runtime can work with, or
    /// that does not fit with another; the reason when the backend cannot
    /// be had, a worker's poller cannot be made or its thread cannot start.
    pub fn build(self) -> io::Result<Runtime> {
        self.validate()?;
        let workers = self.worker_count();
        let stack = self.thread_stack_size.unwrap_or(DEFAULT_STACK);
        let limits = Limits {
            max_nesting: self.max_nesting.unwrap_or(context::DEFAULT_MAX_NESTING),
            buffer_cap: self.io_buffer_cap.unwrap_or(crate::buf::DEFAULT_CAP),
            backend: super::super::backend::for_runtime(self.backend, self.io_uring.as_ref())?,
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
            cpu_mask: match &self.blocking_cores {
                Some(cores) => super::cores::mask_of(cores),
                None => super::current_mask(),
            },
            buffer_cap: limits.buffer_cap,
        });
        static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let (exit_tx, exits) = kovan_channel::unbounded();
        let pin = self.pin_workers.unwrap_or(true);
        let (cores, mask) = match &self.worker_cores {
            Some(list) if pin => (list.clone(), None),
            Some(list) => (Vec::new(), super::cores::mask_of(list)),
            None if pin => (super::allowed_cores(), None),
            None => (Vec::new(), None),
        };
        let capacity = self
            .max_worker_threads
            .unwrap_or(workers.saturating_mul(4))
            .max(workers);
        let shutdown_on_panic = self.unhandled_panic == UnhandledPanic::ShutdownRuntime;
        let mut task_hooks = self.task_hooks;
        super::super::notes::compose(
            &mut task_hooks,
            self.long_poll.is_some(),
            self.record_latencies,
        );
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
                            record_latencies: self.record_latencies,
                            #[cfg(feature = "io-stats")]
                            count_io: self.count_io,
                            ..self.tuning.clone()
                        },
                        seed: self.rng_seed,
                        stack,
                        name: worker_name,
                        cores,
                        mask,
                        handoff_after: self.handoff_after.unwrap_or(Some(DEFAULT_HANDOFF)),
                        long_poll: self.long_poll,
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
                guests: std::sync::OnceLock::new(),
                lingering: super::super::linger::Lingering::new(),
                io: self.io.resolve(),
                buffer_cap: limits.buffer_cap,
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
    /// As for [`build`](Self::build), and `AlreadyExists` when the default
    /// runtime is already running (it was built, or a free function started
    /// it).
    pub fn build_global(self) -> io::Result<RuntimeHandle> {
        if DEFAULT.get().is_some() {
            return Err(already_running());
        }
        let runtime = self.build()?;
        let handle = runtime.handle();
        DEFAULT.set(runtime).map_err(|_| already_running())?;
        Ok(handle)
    }
}

/// The error of a second default runtime.
fn already_running() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "the default runtime is already running",
    )
}

impl fmt::Debug for Builder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("Builder");
        builder
            .field("worker_threads", &self.worker_threads)
            .field("thread_stack_size", &self.thread_stack_size)
            .field("max_nesting", &self.max_nesting)
            .field("max_blocking_threads", &self.max_blocking_threads)
            .field("thread_keep_alive", &self.blocking_keep_alive)
            .field("pin_workers", &self.pin_workers)
            .field("worker_cores", &self.worker_cores)
            .field("blocking_cores", &self.blocking_cores)
            .field("local_spawn_margin", &self.local_spawn_margin)
            .field("io_buffer_cap", &self.io_buffer_cap)
            .field("io_uring", &self.io_uring)
            .field("start_paused", &self.start_paused)
            .field("record_latencies", &self.record_latencies);
        #[cfg(feature = "io-stats")]
        builder.field("count_io", &self.count_io);
        builder.finish_non_exhaustive()
    }
}
