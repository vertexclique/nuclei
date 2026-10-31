//! Runtimes: worker shards, the blocking pool, and where `spawn` puts a task.
//!
//! A runtime has one worker shard per thread it was built with, each a thread
//! running its shard's loop until shutdown, and a blocking pool. `spawn` on a
//! worker keeps the task there unless the worker holds more than a margin
//! of live tasks beyond a shard picked at random, which then takes it. Any
//! other thread has a home shard, where its spawns went last, kept by the
//! same rule (its first spawn takes the less loaded of two distinct shards
//! picked at random, the power of two choices), so tasks spawned together
//! run together. A task never moves after.
//! With no worker threads, a task goes to the shard of the thread that
//! spawns it, and runs while that thread is inside `block_on`.
//!
//! Any thread may call `block_on`. On a worker it runs a nested loop on the
//! worker's shard; elsewhere it runs on the thread's own shard, made on
//! first use, with the runtime set as the one `spawn` uses meanwhile.
//!
//! The process default runtime starts on first use with one worker per core
//! the process may run on and every setting at its default;
//! [`Builder::build_global`] replaces those settings before that.

use core::future::Future;
use std::fmt;
use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use kovan_channel::flavors::RecvDeadline;

use super::blocking::{Pool, PoolConfig};
use super::context;
use super::sched::Sched;
use super::shard::{Limits, Shard, Shared};
use super::task::{self, ScheduleInfo, Task};
use super::workers::{Start, Workers};

mod enter;
mod knobs;
mod panic;
pub use self::enter::{EnterGuard, RuntimeId, TryCurrentError, worker_index};
pub use super::rng::RngSeed;
pub use super::tuning::{TaskMeta, UnhandledPanic};

/// What a runtime's threads share.
pub(crate) struct Inner {
    /// Tells this runtime's workers from another's.
    id: u64,
    pub(crate) workers: Workers,
    pub(crate) pool: Arc<Pool>,
    /// How many live tasks more than the other choice the spawning shard
    /// may hold and still keep a task it spawns.
    local_margin: usize,
    /// The workers' IO runs on rings.
    #[cfg(ring)]
    rings: bool,
    /// The time its tasks and timers read.
    pub(crate) clock: crate::time::clock::Clock,
    /// The shards of threads inside this runtime's `block_on` while its clock
    /// is paused, by address: with the workers, the shards a paused clock
    /// waits for before it moves by itself.
    pub(crate) guests: kovan_map::HashMap<usize, Arc<Shared>>,
    /// The task hooks every task of this runtime carries, if any is set.
    task_hooks: Option<Arc<super::tuning::TaskHooks>>,
    /// The runtime shuts down when a task panics; and once one did.
    shutdown_on_panic: bool,
    panicked: std::sync::atomic::AtomicBool,
    /// Wakes the runtime's `block_on` calls when a task panicked.
    on_panic: crate::sync::Notify,
    /// Seeds the generator of a thread in this runtime's `block_on`.
    rng_seed: Option<RngSeed>,
    /// What dropped handles left to finish, once no worker takes it.
    lingering: super::linger::Lingering,
}

/// Builds a [`Runtime`]. Every setting is optional: an unset one takes the
/// default, which needs no configuration to work well.
///
/// ```
/// let runtime = nuclei::Builder::new().worker_threads(2).build().unwrap();
/// assert_eq!(runtime.block_on(async { 1 + 1 }), 2);
/// ```
#[must_use]
pub struct Builder {
    worker_threads: Option<usize>,
    thread_name: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    thread_stack_size: Option<usize>,
    max_nesting: Option<usize>,
    max_blocking_threads: Option<usize>,
    blocking_keep_alive: Option<Duration>,
    pin_workers: Option<bool>,
    local_spawn_margin: Option<usize>,
    io_buffer_cap: Option<usize>,
    max_worker_threads: Option<usize>,
    backend: Option<crate::IoBackend>,
    handoff_after: Option<Option<Duration>>,
    start_paused: bool,
    tuning: super::tuning::Tuning,
    task_hooks: super::tuning::TaskHooks,
    unhandled_panic: UnhandledPanic,
    rng_seed: Option<RngSeed>,
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
            tuning: super::tuning::Tuning::default(),
            task_hooks: super::tuning::TaskHooks::default(),
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
            backend: super::backend::for_runtime(self.backend)?,
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
                        tuning: super::tuning::Tuning {
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
                    super::tuning::ANY_TASK_HOOKS.store(true, std::sync::atomic::Ordering::Release);
                    Arc::new(task_hooks)
                }),
                shutdown_on_panic,
                panicked: std::sync::atomic::AtomicBool::new(false),
                on_panic: crate::sync::Notify::new(),
                rng_seed: self.rng_seed,
                guests: kovan_map::HashMap::with_capacity(GUESTS_CAPACITY),
                lingering: super::linger::Lingering::new(),
            }
        });
        inner.pool.set_owner(Arc::downgrade(&inner));
        let runtime = Runtime { inner, exits };
        // On an error the runtime drops: the workers already started stop.
        Workers::start(&runtime.inner, workers)?;
        Ok(runtime)
    }

    /// Starts the runtime and makes it the process default, the one the free
    /// functions ([`spawn`], [`block_on`], ...) use outside any other
    /// runtime.
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

/// A runtime: worker shards and a blocking pool. Dropping it stops the
/// workers, drops the tasks they still hold, and waits for the threads to
/// exit; then, on the dropping thread, for what the handles those tasks held
/// left behind (the bytes a socket's write-behind holds, a file's writes) to
/// land, each for at most a minute, so nothing they accepted is lost.
pub struct Runtime {
    inner: Arc<Inner>,
    /// One message per worker that exited.
    exits: kovan_channel::unbounded::Receiver<()>,
}

/// A cloneable handle to a [`Runtime`], to spawn on it from anywhere. It
/// does not keep the runtime running.
#[derive(Clone)]
pub struct RuntimeHandle {
    inner: Arc<Inner>,
}

impl Runtime {
    /// A builder with every setting at its default.
    pub fn builder() -> Builder {
        Builder::new()
    }

    /// Starts a runtime with every setting at its default.
    ///
    /// # Errors
    ///
    /// As for [`Builder::build`].
    pub fn new() -> io::Result<Self> {
        Builder::new().build()
    }

    /// A handle to spawn on this runtime from anywhere.
    pub fn handle(&self) -> RuntimeHandle {
        RuntimeHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Runs `future` to its end on the calling thread; see [`block_on`].
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        block_on_with(&self.inner, future)
    }

    /// Spawns a task on one of the runtime's shards; see [`spawn`].
    pub fn spawn<F>(&self, future: F) -> Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        spawn_with(&self.inner, future)
    }

    /// Runs `f` on the runtime's blocking pool; see [`spawn_blocking`].
    pub fn spawn_blocking<F, T>(&self, f: F) -> Task<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.inner.pool.spawn(f)
    }

    /// Stops the workers and waits for them up to `timeout`; workers still
    /// running a task then are left to exit on their own. What the handles
    /// of the tasks dropped left behind gets the rest of `timeout` to land,
    /// on this thread; what has not by then is given up.
    pub fn shutdown_timeout(mut self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        self.stop_and_join(Some(timeout));
        self.finish_lingering(Some(deadline));
    }

    /// Runs what dropped handles left behind until every part handed to the
    /// runtime ended and nothing more can come (no file write the workers
    /// issued is still to wake its task, no task woken on a stopped worker
    /// is still to be dropped), for at most a linger's time (each part is
    /// bounded by its own) or until `deadline`; on this thread, its runtime
    /// set to this one, so a file's calls run on this blocking pool. Then
    /// what is left, and the lingers handed later, run on threads of their
    /// own.
    fn finish_lingering(&self, deadline: Option<std::time::Instant>) {
        // Once: a shutdown that ran it (or gave it up) leaves its drop none.
        if self.inner.lingering.is_over() {
            return;
        }
        if !self.inner.lingers_done() {
            let linger = std::time::Instant::now() + crate::io::handle::LINGER;
            let deadline = deadline.map_or(linger, |d| d.min(linger));
            let _ = block_on_here(
                &self.inner,
                crate::time::timeout_at(deadline, self.inner.run_lingers()),
            );
        }
        self.inner.lingering.end();
    }

    fn stop_and_join(&mut self, timeout: Option<Duration>) {
        // Lingers handed from now on wait for this drop, never for a worker.
        self.inner.lingering.close();
        self.inner.workers.stop_all();
        let threads = self.inner.workers.take_threads();
        if let Some(timeout) = timeout {
            let deadline = std::time::Instant::now() + timeout;
            // Workers that exited already sent theirs; one message each.
            for thread in &threads {
                if thread.is_finished() {
                    continue;
                }
                if !matches!(self.exits.recv_deadline(deadline), RecvDeadline::Msg(())) {
                    break;
                }
            }
            // A worker that did not exit in time is left to exit on its own.
            for thread in threads {
                if thread.is_finished() {
                    let _ = thread.join();
                }
            }
        } else {
            for thread in threads {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
impl Runtime {
    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop_and_join(None);
        self.finish_lingering(None);
    }
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime")
            .field("worker_threads", &self.inner.workers.running())
            .finish_non_exhaustive()
    }
}

impl RuntimeHandle {
    /// Runs `future` to its end on the calling thread; see [`block_on`].
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        block_on_with(&self.inner, future)
    }

    /// Spawns a task on one of the runtime's shards; see [`spawn`].
    pub fn spawn<F>(&self, future: F) -> Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        spawn_with(&self.inner, future)
    }

    /// Runs `f` on the runtime's blocking pool; see [`spawn_blocking`].
    pub fn spawn_blocking<F, T>(&self, f: F) -> Task<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.inner.pool.spawn(f)
    }

    /// The handle of the runtime this thread is inside, or of the default
    /// runtime (started now if it is not running yet).
    pub fn current() -> Self {
        Self { inner: current() }
    }
}

impl fmt::Debug for RuntimeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeHandle")
            .field("worker_threads", &self.inner.workers.running())
            .finish_non_exhaustive()
    }
}

/// The process default runtime, started on first use.
static DEFAULT: OnceLock<Runtime> = OnceLock::new();

impl Inner {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// What dropped handles left this runtime to finish.
    pub(crate) fn lingering(&self) -> &super::linger::Lingering {
        &self.lingering
    }
}

/// The process default runtime, started now if it is not running.
#[expect(
    clippy::panic,
    reason = "no caller to hand the error to; documented under `# Panics`"
)]
pub(crate) fn default_runtime() -> &'static Runtime {
    DEFAULT.get_or_init(|| {
        Builder::new()
            .build()
            .unwrap_or_else(|err| panic!("the default runtime could not start: {err}"))
    })
}

/// The runtime this thread is inside, or the default one.
pub(crate) fn current() -> Arc<Inner> {
    context::runtime().unwrap_or_else(|| Arc::clone(&default_runtime().inner))
}

/// Runs `f` on the clock of the runtime this thread is inside, or of the
/// default runtime if it runs; `None` when there is neither.
pub(crate) fn with_clock<R>(f: impl FnOnce(Option<&crate::time::clock::Clock>) -> R) -> R {
    match context::runtime() {
        Some(inner) => f(Some(&inner.clock)),
        None => f(DEFAULT.get().map(|rt| &rt.inner.clock)),
    }
}

/// The runtime this thread is inside, or the default one if it runs: never
/// started here (a thread on its way out may ask).
fn running() -> Option<Arc<Inner>> {
    context::runtime().or_else(|| DEFAULT.get().map(|rt| Arc::clone(&rt.inner)))
}

/// [`running`], for a linger's hand-off.
pub(crate) fn running_inner() -> Option<Arc<Inner>> {
    running()
}

/// Spawns a linger on `inner`: on a worker that takes tasks, else (with
/// `here`) on the shard of this thread (which runs it at its next
/// `block_on`); the linger back where neither can take it.
pub(crate) fn spawn_linger(
    inner: &Arc<Inner>,
    linger: super::linger::Guarded,
    here: bool,
) -> Result<(), super::linger::Guarded> {
    if let Some(shard) = inner.workers.place(inner.id, inner.local_margin) {
        spawn_on(inner, shard, linger).detach();
        return Ok(());
    }
    // With no workers, the thread's own shard, if it has one: one is never
    // made here (a thread on its way out may hand a linger).
    let running = context::current(|current| current.is_some());
    if here && (running || context::own_shard_if_made().is_some()) {
        let _runtime = context::set_runtime(inner);
        spawn_here(linger, false).detach();
        return Ok(());
    }
    Err(linger)
}

/// A shard to serve a thread that runs none, of the runtime this thread is
/// inside or of the default one, if one runs: it never starts one (a thread
/// on its way out may ask).
pub(crate) fn running_service_shard() -> Option<Arc<Shared>> {
    let inner = running()?;
    inner
        .workers
        .place(inner.id, inner.local_margin)
        .map(Arc::clone)
}

/// Whether the shard [`service_shard`] hands out runs a ring: the current
/// (or default) runtime's workers do, or with no workers, this thread's own
/// shard.
#[cfg(ring)]
pub(crate) fn service_rings() -> bool {
    let inner = current();
    if inner.workers.running() > 0 {
        inner.rings
    } else {
        super::backend::chosen().is_ok_and(|b| b.rings())
    }
}

/// A shard to serve a thread that runs none (its timers, the registrations
/// of its IO): one of the current (or default) runtime's workers, the less
/// loaded of two; with no workers, the calling thread's own shard.
pub(crate) fn service_shard() -> Arc<Shared> {
    let inner = current();
    match inner.workers.place(inner.id, inner.local_margin) {
        Some(shard) => Arc::clone(shard),
        None => Arc::clone(&own_shard().shared),
    }
}

/// This thread's own shard, started now if it is not running.
#[expect(
    clippy::panic,
    reason = "no caller to hand the error to; documented under `# Panics`"
)]
fn own_shard() -> std::rc::Rc<Shard> {
    match context::own_shard() {
        Ok(shard) => shard,
        Err(err) => panic!("this thread's shard could not start: {err}"),
    }
}

/// Spawns `future` on `inner`.
fn spawn_with<F>(inner: &Inner, future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let Some(shard) = inner.workers.place(inner.id, inner.local_margin) else {
        // No worker takes tasks: the spawning thread's own shard runs it.
        return spawn_here(future, false);
    };
    spawn_on(inner, shard, future)
}

/// Spawns `future` on `inner`'s worker `shard`.
fn spawn_on<F>(inner: &Inner, shard: &Arc<Shared>, future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let hooks = inner.task_hooks.clone();
    let (runnable, task) = task::spawn(
        future,
        Sched::counted(Arc::clone(shard), None, hooks.clone()),
    );
    spawned(hooks.as_deref(), &task);
    // As its schedule would, without the turn through the task's table.
    if let Some(runnable) = super::sched::queue_here(shard, runnable, ScheduleInfo::new(false)) {
        runnable.schedule();
    }
    task
}

/// Calls the spawn hook, if the task's runtime has one.
fn spawned<T>(hooks: Option<&super::tuning::TaskHooks>, task: &Task<T>) {
    if let Some(hooks) = hooks {
        super::tuning::call(hooks.spawn.as_ref(), task.id());
    }
}

/// The task hooks of the runtime set on this thread, looked for only while
/// some runtime of the process has any.
fn context_hooks() -> Option<Arc<super::tuning::TaskHooks>> {
    if !super::tuning::ANY_TASK_HOOKS.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    context::runtime().and_then(|inner| inner.task_hooks.clone())
}

/// Spawns `future` on the shard of the calling thread: the worker it runs,
/// or the thread's own. `bound`: the future is not `Send` and must stay.
fn spawn_here<F>(future: F, bound: bool) -> Task<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let place = |shard: &Shard| {
        (
            Arc::clone(&shard.shared),
            bound.then(|| context::home_for(shard)),
        )
    };
    let (shared, home) =
        context::current(|current| current.map(place)).unwrap_or_else(|| place(&own_shard()));
    let hooks = context_hooks();
    let (runnable, task) = if let Some(home) = home {
        // SAFETY: the task's schedule sends a woken bound task only to its
        // home's thread (leaked rather than dropped if that thread is gone);
        // the future is 'static.
        unsafe { task::spawn_unchecked(future, Sched::counted(shared, Some(home), hooks.clone())) }
    } else {
        // SAFETY: the task's schedule sends a woken task of this shard back
        // to it; the future is 'static, and `Send` (a caller with `bound`
        // false is `spawn`'s, whose future is).
        unsafe { task::spawn_unchecked(future, Sched::counted(shared, None, hooks.clone())) }
    };
    spawned(hooks.as_deref(), &task);
    runnable.schedule();
    task
}

/// Runs `future` on the calling thread with `inner` as the runtime `spawn`
/// uses meanwhile.
fn block_on_with<F: Future>(inner: &Arc<Inner>, future: F) -> F::Output {
    if inner.shutdown_on_panic {
        return block_on_here(inner, panic::watched(inner, future));
    }
    block_on_here(inner, future)
}

fn block_on_here<F: Future>(inner: &Arc<Inner>, future: F) -> F::Output {
    let _runtime = context::set_runtime(inner);
    // Pinned here once: the calls below take it by reference.
    let mut future = core::pin::pin!(future);
    // On the shard this thread runs, the call nests, inside the shard.
    if let Some(output) =
        context::current(|current| current.map(|shard| shard.block_on(future.as_mut())))
    {
        return output;
    }
    let shard = own_shard();
    let _current = context::enter(&shard);
    let _guest = super::paused::Guest::enter(inner, &shard.shared);
    // A seeded runtime's choices made here repeat, call after call.
    let _seeded = inner
        .rng_seed
        .map(|seed| super::rng::Reseeded::new(seed, 0));
    #[cfg(port)]
    shard.take_port_back();
    shard.block_on_then_leave(future.as_mut())
}

/// Runs `future` to its end on the calling thread, running other tasks of
/// the thread's shard meanwhile.
///
/// On a worker thread (inside a task) the call nests: the worker's shard
/// keeps running its other tasks, timers and IO until `future` completes.
/// The task that called it is not polled again until the call returns, so a
/// `future` that waits for that very task never completes.
///
/// # Panics
///
/// When nested deeper than the runtime's `max_nesting` (256 by default), or
/// when the thread's shard cannot start (its poller cannot be made).
pub fn block_on<F: Future>(future: F) -> F::Output {
    block_on_with(&current(), future)
}

/// Spawns a task on the runtime this thread is inside, or on the default
/// runtime. Spawned on a worker, the task stays on that worker unless it
/// holds more live tasks than another shard by the runtime's
/// [`local_spawn_margin`](Builder::local_spawn_margin); spawned anywhere
/// else, it goes to the spawning thread's home shard by the same rule (the
/// first spawn takes the less loaded of two shards picked at random). The
/// task stays on its shard.
///
/// Dropping the returned [`Task`] cancels it; call [`Task::detach`] to let
/// it run on its own.
///
/// # Panics
///
/// When the default runtime or this thread's shard cannot start (its
/// poller cannot be made); [`task::Builder::spawn`](crate::task::Builder::spawn)
/// answers the error instead.
#[inline]
pub fn spawn<F>(future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // With no task hooks nothing the spawn runs is the user's, so nothing
    // sets or unsets the thread's runtime meanwhile: it is used in place.
    let spawned = context::with_runtime(|inner| match inner {
        Some(inner) if inner.task_hooks.is_none() => Ok(spawn_with(inner, future)),
        _ => Err(future),
    });
    spawned.unwrap_or_else(|future| spawn_with(&current(), future))
}

/// Spawns a task that stays on the calling thread: a worker's shard, or the
/// thread's own (where it runs while the thread is inside `block_on`). The
/// future need not be `Send`.
///
/// # Panics
///
/// When this thread's shard cannot start (its poller cannot be made);
/// [`task::Builder::spawn_local`](crate::task::Builder::spawn_local) answers
/// the error instead.
pub fn spawn_local<F>(future: F) -> Task<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    spawn_here(future, true)
}

/// Runs `f` on the blocking pool of the runtime this thread is inside, or of
/// the default runtime, so it never holds a shard up.
///
/// # Panics
///
/// When the default runtime cannot start.
pub fn spawn_blocking<F, T>(f: F) -> Task<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    current().pool.spawn(f)
}

/// Runs `future` as a task of the runtime's own, left out of its shard's
/// count of live tasks so it keeps no worker from draining: on the shard the
/// calling thread runs, else on a shard that serves threads that run none.
#[cfg(unix)]
pub(crate) fn spawn_service<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let shared = context::current(|current| current.map(|s| Arc::clone(&s.shared)))
        .unwrap_or_else(service_shard);
    // SAFETY: as in `spawn_here`: the schedule sends a woken task back to
    // its shard; the future is `Send` and 'static.
    let (runnable, task) = unsafe {
        // The runtime's own service task: not a task of its user's.
        task::spawn_unchecked(future, Sched::uncounted(shared))
    };
    runnable.schedule();
    task.detach();
}

/// Runs `f` on the blocking pool of the runtime this thread is inside, if
/// it is inside one (it never starts the default runtime, which a thread on
/// its way out could not): whether `f` went.
#[cfg(port)]
pub(crate) fn spawn_blocking_inside(f: impl FnOnce() + Send + 'static) -> bool {
    match blocking_pool_inside() {
        Some(pool) => {
            pool.spawn(f).detach();
            true
        }
        None => false,
    }
}

/// The blocking pool of the runtime this thread is inside, if it is inside one; never the
/// default runtime's, which it does not start.
#[cfg(completion)]
pub(crate) fn blocking_pool_inside() -> Option<Arc<super::blocking::Pool>> {
    context::runtime().map(|inner| Arc::clone(&inner.pool))
}

/// The cores this process may run on, in order (Linux; Windows, in the
/// process's processor group); empty where that is not known, so nothing
/// is pinned.
pub(crate) fn allowed_cores() -> Vec<usize> {
    #[cfg(target_os = "linux")]
    {
        current_mask().map_or_else(Vec::new, |set| {
            (0..libc::CPU_SETSIZE as usize)
                // SAFETY: a plain read of a set of our own.
                .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
                .collect()
        })
    }
    #[cfg(windows)]
    {
        use crate::syscore::windows::{GetCurrentProcess, GetProcessAffinityMask};
        let (mut process, mut system) = (0usize, 0usize);
        // SAFETY: plain out-parameters of our own.
        if unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) } != 0 {
            return (0..usize::BITS as usize)
                .filter(|&cpu| process & (1 << cpu) != 0)
                .collect();
        }
        Vec::new()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Vec::new()
    }
}

/// A thread's CPU affinity mask.
#[cfg(target_os = "linux")]
pub(crate) type CpuMask = libc::cpu_set_t;
/// A thread's CPU affinity mask; none is kept where threads are not pinned.
#[cfg(not(target_os = "linux"))]
pub(crate) type CpuMask = ();

/// The calling thread's affinity mask, `None` where it is not known.
pub(crate) fn current_mask() -> Option<CpuMask> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a plain syscall into a zeroed set of our own.
        unsafe {
            let mut set: libc::cpu_set_t = core::mem::zeroed();
            (libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) == 0).then_some(set)
        }
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// Runs the calling thread on `mask`. A failure leaves the mask it had,
/// which costs locality and nothing else.
pub(crate) fn apply_mask(mask: &CpuMask) {
    #[cfg(target_os = "linux")]
    // SAFETY: a plain syscall with a set of our own.
    let _ = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), mask) };
    #[cfg(not(target_os = "linux"))]
    let _ = mask;
}

/// Pins the calling thread to `core` (Linux, Windows). A failure leaves it
/// unpinned, which costs locality and nothing else.
pub(crate) fn pin_to(core: usize) {
    #[cfg(target_os = "linux")]
    // SAFETY: a plain syscall with a set of our own.
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        let _ = libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set);
    }
    #[cfg(windows)]
    if core < usize::BITS as usize {
        use crate::syscore::windows::{GetCurrentThread, SetThreadAffinityMask};
        // SAFETY: the pseudo-handle of the calling thread, and a mask.
        unsafe { SetThreadAffinityMask(GetCurrentThread(), 1 << core) };
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    let _ = core;
}
