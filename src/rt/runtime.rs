//! Runtimes: worker shards, the blocking pool, and where `spawn` puts a task.
//!
//! A runtime has one worker shard per thread it was built with, each a thread
//! running its shard's loop until shutdown, and a blocking pool. `spawn` on a
//! worker keeps the task there unless the worker holds more than a margin
//! of live tasks beyond a shard picked at random, which then takes it; from
//! any other thread it picks two distinct shards at random and takes the one
//! with fewer live tasks (the power of two choices). A task never moves
//! after.
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
use std::thread::JoinHandle;
use std::time::Duration;

use kovan_channel::flavors::RecvDeadline;

use super::blocking::{Pool, PoolConfig};
use super::context;
use super::sched::{Counted, Sched};
use super::shard::{Limits, Shard, Shared};
use super::task::{self, Task};
use crate::sync::internal::Ordering;

/// What a runtime's threads share.
pub(crate) struct Inner {
    /// Tells this runtime's workers from another's.
    id: u64,
    shards: Box<[Arc<Shared>]>,
    pool: Arc<Pool>,
    /// How many live tasks more than the other choice the spawning shard
    /// may hold and still keep a task it spawns.
    local_margin: usize,
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

    /// How many more live tasks than another shard a worker may hold and
    /// still keep a task it spawns (default 32). A task spawned on a worker
    /// stays on it (so a task that waits for what it spawned keeps it on
    /// its own thread) unless the worker holds that many more tasks than a
    /// shard picked at random; then it goes to the less loaded of the two.
    /// Zero balances strictly; a larger margin keeps more work local.
    pub fn local_spawn_margin(mut self, margin: usize) -> Self {
        self.local_spawn_margin = Some(margin);
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
        };
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
        let shards = (0..workers)
            .map(|_| Shard::new(limits))
            .collect::<io::Result<Vec<_>>>()?;
        static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let inner = Arc::new(Inner {
            id: IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            shards: shards.iter().map(|s| Arc::clone(&s.shared)).collect(),
            pool,
            local_margin: self.local_spawn_margin.unwrap_or(DEFAULT_LOCAL_MARGIN),
        });
        let (exit_tx, exits) = kovan_channel::unbounded();
        let cores = if self.pin_workers.unwrap_or(true) {
            allowed_cores()
        } else {
            Vec::new()
        };
        let mut threads = Vec::with_capacity(workers);
        for (index, shard) in shards.into_iter().enumerate() {
            let worker_inner = Arc::clone(&inner);
            let exit_tx = exit_tx.clone();
            let core = (!cores.is_empty()).then(|| cores[index % cores.len()]);
            let spawned = std::thread::Builder::new()
                .name(worker_name())
                .stack_size(stack)
                .spawn(move || {
                    /// Reports the worker's exit once its shard is dropped,
                    /// even when the loop ends by a panic.
                    struct Exit(kovan_channel::unbounded::Sender<()>);
                    impl Drop for Exit {
                        fn drop(&mut self) {
                            self.0.send(());
                        }
                    }
                    let _exit = Exit(exit_tx);
                    if let Some(core) = core {
                        pin_to(core);
                    }
                    shard.set_owner(worker_inner.id, index);
                    let _runtime = context::set_runtime(&worker_inner);
                    let _current = context::enter(&shard);
                    shard.block_on(shard.shared.stopped());
                });
            match spawned {
                Ok(handle) => threads.push(handle),
                Err(err) => {
                    // The workers already started stop again.
                    inner.shards.iter().for_each(|s| s.stop());
                    threads.into_iter().for_each(|t: JoinHandle<()>| {
                        let _ = t.join();
                    });
                    return Err(err);
                }
            }
        }
        Ok(Runtime {
            inner,
            threads,
            exits,
        })
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
            .finish_non_exhaustive()
    }
}

/// A runtime: worker shards and a blocking pool. Dropping it stops the
/// workers, drops the tasks they still hold, and waits for the threads to
/// exit.
pub struct Runtime {
    inner: Arc<Inner>,
    threads: Vec<JoinHandle<()>>,
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
        self.inner.shards.iter().for_each(|s| s.stop());
        if let Some(timeout) = timeout {
            let deadline = std::time::Instant::now() + timeout;
            for _ in 0..self.threads.len() {
                if !matches!(self.exits.recv_deadline(deadline), RecvDeadline::Msg(())) {
                    break;
                }
            }
            // A worker that did not exit in time is left to exit on its own.
            for thread in self.threads.drain(..) {
                if thread.is_finished() {
                    let _ = thread.join();
                }
            }
        } else {
            for thread in self.threads.drain(..) {
                let _ = thread.join();
            }
        }
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
            .field("worker_threads", &self.inner.shards.len())
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
            .field("worker_threads", &self.inner.shards.len())
            .finish_non_exhaustive()
    }
}

/// The process default runtime, started on first use.
static DEFAULT: OnceLock<Runtime> = OnceLock::new();

/// The runtime this thread is inside, or the default one.
fn current() -> Arc<Inner> {
    context::runtime().unwrap_or_else(|| {
        let runtime = DEFAULT.get_or_init(|| {
            Builder::new()
                .build()
                .unwrap_or_else(|err| panic!("the default runtime could not start: {err}"))
        });
        Arc::clone(&runtime.inner)
    })
}

/// Picks the shard a new task goes to. Spawned on one of the runtime's
/// workers: that worker, unless it holds more than `local_margin` live tasks
/// beyond a shard picked at random, which then takes it. Spawned anywhere
/// else: the less loaded of two distinct shards picked at random.
fn place(inner: &Inner) -> &Arc<Shared> {
    let shards = &inner.shards;
    let n = shards.len();
    if n == 1 {
        return &shards[0];
    }
    if let Some(local) = context::current(|current| current.and_then(|s| s.worker_index(inner.id)))
    {
        let other = (local + 1 + random_below(n - 1)) % n;
        let (ll, lo) = (
            shards[local].load.load(Ordering::Relaxed),
            shards[other].load.load(Ordering::Relaxed),
        );
        return if ll <= lo.saturating_add(inner.local_margin) {
            &shards[local]
        } else {
            &shards[other]
        };
    }
    let a = random_below(n);
    let b = (a + 1 + random_below(n - 1)) % n;
    let (la, lb) = (
        shards[a].load.load(Ordering::Relaxed),
        shards[b].load.load(Ordering::Relaxed),
    );
    if la <= lb { &shards[a] } else { &shards[b] }
}

/// A random number below `n`, from a per-thread xorshift generator.
fn random_below(n: usize) -> usize {
    use core::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new({
            // Seeded from the thread's own address and the clock: distinct
            // per thread, and nothing depends on its quality but balance.
            let local = 0u8;
            let addr = &local as *const u8 as u64;
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            (addr ^ time.rotate_left(32)) | 1
        });
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        // Multiply-shift: unbiased enough for placement, no division.
        ((u128::from(x) * n as u128) >> 64) as usize
    })
}

/// A shard to fire a timer for a thread that runs none: one of the current
/// (or default) runtime's workers, the less loaded of two; with no workers,
/// the calling thread's own shard.
pub(crate) fn timer_shard() -> Arc<Shared> {
    let inner = current();
    if inner.shards.is_empty() {
        return match context::own_shard() {
            Ok(shard) => Arc::clone(&shard.shared),
            Err(err) => panic!("this thread's shard could not start: {err}"),
        };
    }
    Arc::clone(place(&inner))
}

/// Spawns `future` on `inner`.
fn spawn_with<F>(inner: &Arc<Inner>, future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if inner.shards.is_empty() {
        // No workers: the spawning thread's own shard runs it.
        return spawn_here(future, false);
    }
    let shard = place(inner);
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
    shard.block_on(future)
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
/// else, it goes to the less loaded of two shards picked at random. The task
/// stays on its shard.
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

/// The cores this process may run on, in order (Linux); empty where that is
/// not known, so nothing is pinned.
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
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Pins the calling thread to `core` (Linux). A failure leaves it unpinned,
/// which costs locality and nothing else.
fn pin_to(core: usize) {
    #[cfg(target_os = "linux")]
    // SAFETY: a plain syscall with a set of our own.
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        let _ = libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = core;
}
