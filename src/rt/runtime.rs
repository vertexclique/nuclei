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
use super::sched::{Counted, Sched};
use super::shard::{Limits, Shard, Shared};
use super::task::{self, Task};
use super::workers::{Start, Workers};

/// What a runtime's threads share.
pub(crate) struct Inner {
    /// Tells this runtime's workers from another's.
    id: u64,
    pub(crate) workers: Workers,
    pool: Arc<Pool>,
    /// How many live tasks more than the other choice the spawning shard
    /// may hold and still keep a task it spawns.
    local_margin: usize,
    /// The workers' IO runs on rings.
    #[cfg(ring)]
    rings: bool,
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

impl Builder {
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
    /// 8 MiB).
    pub fn thread_stack_size(mut self, bytes: usize) -> Self {
        self.thread_stack_size = Some(bytes);
        self
    }

    /// How deep `block_on` may nest on one thread (default 256). A call one
    /// level deeper panics instead of running the thread out of stack.
    pub fn max_nesting(mut self, depth: usize) -> Self {
        self.max_nesting = Some(depth);
        self
    }

    /// The most threads the blocking pool runs at once (default 512).
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
    /// may add up to. A stopped worker's place is not taken again.
    pub fn max_worker_threads(mut self, n: usize) -> Self {
        self.max_worker_threads = Some(n);
        self
    }

    /// The most bytes each worker's IO buffers hold (default 64 MiB). Reads
    /// that had to wait land in these buffers, and writes that had to wait
    /// are copied into them; at the cap, IO waits for buffers to come back,
    /// so a worker's IO memory never grows past it.
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
            max_threads: self
                .max_blocking_threads
                .unwrap_or(DEFAULT_BLOCKING_THREADS)
                .max(1),
            keep_alive: self.blocking_keep_alive.unwrap_or(DEFAULT_KEEP_ALIVE),
            stack_size: self.thread_stack_size,
            name: blocking_name,
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
        let inner = Arc::new(Inner {
            id: IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            workers: Workers::new(
                capacity,
                workers,
                Start {
                    limits,
                    stack,
                    name: worker_name,
                    cores,
                },
                exit_tx,
            ),
            pool,
            local_margin: self.local_spawn_margin.unwrap_or(DEFAULT_LOCAL_MARGIN),
            #[cfg(ring)]
            rings,
        });
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
            .finish_non_exhaustive()
    }
}

/// A runtime: worker shards and a blocking pool. Dropping it stops the
/// workers, drops the tasks they still hold, and waits for the threads to
/// exit.
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
    /// running a task then are left to exit on their own.
    pub fn shutdown_timeout(mut self, timeout: Duration) {
        self.stop_and_join(Some(timeout));
    }

    fn stop_and_join(&mut self, timeout: Option<Duration>) {
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
}

/// The process default runtime, started now if it is not running.
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
        None => match context::own_shard() {
            Ok(shard) => Arc::clone(&shard.shared),
            Err(err) => panic!("this thread's shard could not start: {err}"),
        },
    }
}

/// Spawns `future` on `inner`.
fn spawn_with<F>(inner: &Arc<Inner>, future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let Some(shard) = inner.workers.place(inner.id, inner.local_margin) else {
        // No worker takes tasks: the spawning thread's own shard runs it.
        return spawn_here(future, false);
    };
    let (runnable, task) = task::spawn(
        Counted::new(future, shard),
        Sched {
            shard: Arc::clone(shard),
            bound: false,
        },
    );
    runnable.schedule();
    task
}

/// Spawns `future` on the shard of the calling thread: the worker it runs,
/// or the thread's own. `bound`: the future is not `Send` and must stay.
fn spawn_here<F>(future: F, bound: bool) -> Task<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let shared =
        context::current(|current| current.map(|s| Arc::clone(&s.shared))).unwrap_or_else(|| {
            match context::own_shard() {
                Ok(shard) => Arc::clone(&shard.shared),
                Err(err) => panic!("this thread's shard could not start: {err}"),
            }
        });
    // SAFETY: the task's schedule sends a woken task of this shard back to
    // it, and a bound task only to the shard's own thread (its local inbox,
    // leaked rather than dropped if that thread is gone); the future is
    // 'static.
    let (runnable, task) = unsafe {
        task::spawn_unchecked(
            Counted::new(future, &shared),
            Sched {
                shard: shared,
                bound,
            },
        )
    };
    runnable.schedule();
    task
}

/// Runs `future` on the calling thread with `inner` as the runtime `spawn`
/// uses meanwhile.
fn block_on_with<F: Future>(inner: &Arc<Inner>, future: F) -> F::Output {
    let _runtime = context::set_runtime(inner);
    let running: *const Shard =
        context::current(|current| current.map_or(core::ptr::null(), |s| s as *const Shard));
    if !running.is_null() {
        // SAFETY: the shard this thread runs is kept alive by the guard that
        // entered it, further up this thread's stack, for longer than this
        // call.
        return unsafe { (*running).block_on(future) };
    }
    let shard = match context::own_shard() {
        Ok(shard) => shard,
        Err(err) => panic!("this thread's shard could not start: {err}"),
    };
    let _current = context::enter(&shard);
    #[cfg(port)]
    shard.take_port_back();
    let output = shard.block_on(future);
    shard.leave();
    output
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
pub fn spawn<F>(future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_with(&current(), future)
}

/// Spawns a task that stays on the calling thread: a worker's shard, or the
/// thread's own (where it runs while the thread is inside `block_on`). The
/// future need not be `Send`.
pub fn spawn_local<F>(future: F) -> Task<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    spawn_here(future, true)
}

/// Runs `f` on the blocking pool of the runtime this thread is inside, or of
/// the default runtime, so it never holds a shard up.
pub fn spawn_blocking<F, T>(f: F) -> Task<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    current().pool.spawn(f)
}

/// Runs `f` on the blocking pool of the runtime this thread is inside, if
/// it is inside one (it never starts the default runtime, which a thread on
/// its way out could not): whether `f` went.
#[cfg(port)]
pub(crate) fn spawn_blocking_inside(f: impl FnOnce() + Send + 'static) -> bool {
    match context::runtime() {
        Some(inner) => {
            inner.pool.spawn(f).detach();
            true
        }
        None => false,
    }
}

/// The cores this process may run on, in order (Linux; Windows, in the
/// process's processor group); empty where that is not known, so nothing
/// is pinned.
fn allowed_cores() -> Vec<usize> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a plain syscall into a zeroed set of our own.
        unsafe {
            let mut set: libc::cpu_set_t = core::mem::zeroed();
            if libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) == 0 {
                return (0..libc::CPU_SETSIZE as usize)
                    .filter(|&cpu| libc::CPU_ISSET(cpu, &set))
                    .collect();
            }
        }
        Vec::new()
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
