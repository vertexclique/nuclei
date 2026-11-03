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
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

use kovan_channel::flavors::RecvDeadline;

use super::blocking::Pool;
use super::context;
use super::handoff::life::Life;
use super::sched::Sched;
use super::shard::{Shard, Shared};
use super::task::{self, ScheduleInfo, Task};
use super::workers::{TEARDOWN_WAIT, Workers};

mod builder;
mod cores;
mod enter;
mod fallible;
pub(crate) mod io_knobs;
mod knobs;
mod panic;
mod placement;
mod stats;
pub use self::builder::Builder;
pub use self::enter::{EnterGuard, RuntimeId, TryCurrentError, worker_index};
pub(crate) use self::fallible::{try_current, try_spawn, try_spawn_blocking, try_spawn_local};
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
    guests: OnceLock<kovan_map::HashMap<usize, Arc<Shared>>>,
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
    /// What the runtime's handles read of its builder.
    pub(crate) io: io_knobs::IoKnobs,
    /// What each worker's IO buffers may hold, and the pool of a thread that serves the
    /// runtime but runs no shard (`buf::with_pool`).
    pub(crate) buffer_cap: usize,
}

/// A runtime: worker shards and a blocking pool. Dropping it stops the
/// workers, drops the tasks they still hold, and waits for the threads to
/// exit (every one but the dropping thread, when a task of the runtime drops
/// it, which goes on once the drop returns, and a thread still in the poll its
/// shard was taken in, which it waits for one second and then says so); then, on the dropping thread,
/// for what the handles those tasks held left behind (the bytes a socket's
/// write-behind holds, a file's writes) to land, each for at most a minute,
/// so nothing they accepted is lost.
pub struct Runtime {
    inner: Arc<Inner>,
    /// One message per runtime thread that exited, naming it.
    exits: kovan_channel::unbounded::Receiver<ThreadId>,
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
    ///
    /// Called from a task of another runtime, `future` keeps this runtime's
    /// clock and timers meanwhile; a sleep made inside it and polled after the
    /// call returned reads as elapsed. On a worker's shard, whose polls the
    /// handoff may take, the nested runtime's sleeps share the worker's wheels
    /// and fire by the worker's runtime's clock; on a thread's own shard each
    /// runtime keeps its own.
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

    /// Stops the workers and waits for them up to `timeout` (a thread still
    /// in the poll its shard was taken in, too: a drop without a timeout
    /// waits for it one second); workers still running a task then are left
    /// to exit on their own, and it is said. What the handles
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
            let linger = std::time::Instant::now() + self.inner.io.linger;
            let deadline = deadline.map_or(linger, |d| d.min(linger));
            let _ = block_on_here(
                &self.inner,
                crate::time::timeout_at(deadline, self.inner.run_lingers()),
            );
        }
        self.inner.lingering.end();
    }

    fn stop_and_join(&mut self, timeout: Option<Duration>) {
        let now = std::time::Instant::now();
        let deadline = timeout.map(|timeout| now + timeout);
        self.inner
            .workers
            .set_teardown_by(now + timeout.unwrap_or(TEARDOWN_WAIT));
        // Lingers handed from now on wait for this drop, never for a worker.
        self.inner.lingering.close();
        self.inner.workers.stop_all();
        // The monitor first: a pass under way as it closed may still take a
        // shard and start a runner for it. Once it ended, the list taken next
        // holds every thread to wait for, a detached one marked so
        // (proofs/tla/teardown, `JoinsNoDetached`, `StartedJoined`); the take
        // stops a worker started since the stop above, and none starts after
        // it (proofs/tla/teardown/Grow.tla).
        let mut exited = Vec::new();
        if let Some(monitor) = self.inner.workers.take_monitor() {
            self.join(vec![monitor], deadline, &mut exited);
        }
        #[cfg(test)]
        teardown_pause(false);
        let taken = self.inner.workers.take_threads();
        #[cfg(test)]
        teardown_pause(true);
        self.join(taken.joined, deadline, &mut exited);
        self.join_detached(taken.detached, &mut exited);
    }

    /// Waits for the threads still in the poll their shard was taken in until the teardown
    /// deadline, and says so if one is still in it then
    /// (proofs/tla/teardown/Teardown.tla, `D_wait_detached`).
    fn join_detached(&self, threads: Vec<(JoinHandle<()>, Arc<Life>)>, exited: &mut Vec<ThreadId>) {
        if threads.is_empty() {
            return;
        }
        let (handles, lives): (Vec<_>, Vec<_>) = threads.into_iter().unzip();
        self.join(handles, Some(self.inner.workers.teardown_by()), exited);
        let still = lives.iter().filter(|life| life.is_detached()).count();
        if still != 0 {
            log::warn!(
                "{still} runtime threads were still in a task's poll at the shutdown deadline; the tasks they hold end when those polls return"
            );
        }
    }

    /// Waits for `threads` to exit, never for the calling thread's own (a
    /// runtime dropped in one of its own tasks would join itself:
    /// proofs/tla/teardown, `NoSelfJoin`); with a `deadline`, until it
    /// passes, a thread still running then left to exit on its own, and
    /// `exited` the threads whose exit came, kept from one call to the next.
    fn join(
        &self,
        threads: Vec<JoinHandle<()>>,
        deadline: Option<std::time::Instant>,
        exited: &mut Vec<ThreadId>,
    ) {
        let me = std::thread::current().id();
        let threads = threads
            .into_iter()
            .filter(|thread| thread.thread().id() != me);
        let Some(deadline) = deadline else {
            for thread in threads {
                let _ = thread.join();
            }
            return;
        };
        let threads: Vec<_> = threads.collect();
        // Until each thread read as finished or its exit came, which it sends
        // just before: a message only wakes the wait, as it may be another
        // thread's (a detached one's, one that exited before), and one that
        // came while an earlier list was waited for counts too
        // (proofs/tla/teardown/Join.tla, `WaitsForAll`, `Returns`).
        while threads
            .iter()
            .any(|thread| !thread.is_finished() && !exited.contains(&thread.thread().id()))
        {
            match self.exits.recv_deadline(deadline) {
                RecvDeadline::Msg(id) => exited.push(id),
                _ => break,
            }
        }
        // A thread that did not exit in time is left to exit on its own.
        for thread in threads {
            if thread.is_finished() {
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

/// What a teardown runs at one moment, and whether that is after its take.
#[cfg(test)]
type Pause = (bool, Box<dyn FnOnce()>);

#[cfg(test)]
thread_local! {
    /// What a drop on this thread runs at one moment of its teardown, for a
    /// test to start a worker there: just before its take of the threads, or
    /// (`true`) just after.
    pub(crate) static TEARDOWN_PAUSE: core::cell::RefCell<Option<Pause>> =
        const { core::cell::RefCell::new(None) };
}

/// Runs what [`TEARDOWN_PAUSE`] holds for this moment of a teardown.
#[cfg(test)]
fn teardown_pause(after_take: bool) {
    let pause =
        TEARDOWN_PAUSE.with(|pause| pause.borrow_mut().take_if(|(at, _)| *at == after_take));
    if let Some((_, run)) = pause {
        run();
    }
}

#[cfg(all(test, not(loom)))]
mod tests_drop;
#[cfg(all(test, not(loom)))]
mod tests_nested;

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

    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
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
    /// The guests, made at the first join: a runtime whose clock never pauses has none.
    pub(crate) fn guests(&self) -> &kovan_map::HashMap<usize, Arc<Shared>> {
        self.guests
            .get_or_init(|| kovan_map::HashMap::with_capacity(builder::GUESTS_CAPACITY))
    }

    /// The guests, if any ever joined.
    pub(crate) fn guests_made(&self) -> Option<&kovan_map::HashMap<usize, Arc<Shared>>> {
        self.guests.get()
    }
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
    fallible::try_default()
        .unwrap_or_else(|err| panic!("the default runtime could not start: {err}"))
}

/// The runtime this thread is inside, or the default one.
impl Inner {
    /// Another counted reference to this runtime: every `Inner` lives in the `Arc` its builder
    /// made (`Builder::build`).
    fn arc(&self) -> Arc<Inner> {
        // SAFETY: `self` is borrowed from that live `Arc`, so its count is above zero.
        unsafe {
            Arc::increment_strong_count(self);
            Arc::from_raw(self)
        }
    }
}

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

/// How long the process's exit waits for files' lingers: the default
/// runtime's linger time, or the default's default.
pub(crate) fn exit_linger() -> std::time::Duration {
    DEFAULT
        .get()
        .map_or(crate::io::handle::LINGER, |rt| rt.inner.io.linger)
}

/// Spawns a linger on `inner`: on a worker that takes tasks, else on the
/// linger thread, still counted by `inner` until it ends; never on this
/// thread's own shard, which runs only at the thread's next `block_on`, if
/// one ever comes (proofs/tla/going/Unworked.tla).
pub(crate) fn spawn_linger(inner: &Arc<Inner>, linger: super::linger::Guarded) {
    match inner.workers.place(inner.id, inner.local_margin) {
        Some(shard) => spawn_on(inner, shard, linger).detach(),
        None => super::linger::on_thread(linger),
    }
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
fn own_shard() -> std::rc::Rc<Shard> {
    context::own_shard_if_made().unwrap_or_else(start_own_shard)
}

/// [`own_shard`] on a thread that has none yet.
#[cold]
#[inline(never)]
#[expect(
    clippy::panic,
    reason = "no caller to hand the error to; documented under `# Panics`"
)]
fn start_own_shard() -> std::rc::Rc<Shard> {
    match context::own_shard() {
        Ok(shard) => shard,
        Err(err) => panic!("this thread's shard could not start: {err}"),
    }
}

/// Spawns `future` on `inner`.
#[inline(always)]
fn spawn_with<F>(inner: &Inner, future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let Some(shard) = inner.workers.place(inner.id, inner.local_margin) else {
        return spawn_unworked(inner, future);
    };
    spawn_on(inner, shard, future)
}

/// Spawns `future` of `inner`, which has no worker to take it, on the spawning thread's shard.
/// Where that shard's loop serves another runtime (`inner` runs nested on it) the task is a
/// visitor, polled and dropped with `inner` current (`proofs/tla/nested/Nested.tla`).
fn spawn_unworked<F>(inner: &Inner, future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let hooks =
        if context::current(|current| current.is_some_and(|shard| shard.serves_other(inner))) {
            Some(visitor_hooks(inner))
        } else {
            context_hooks()
        };
    spawn_here::<_, false>(future, hooks)
}

/// The hooks of a visitor of `inner`: its runtime's own, and `inner` to make current around its
/// polls. The visitor is a mark on its schedule, not a wrapper of its future, so a future's poll
/// keeps the one caller it has.
#[cold]
#[inline(never)]
fn visitor_hooks(inner: &Inner) -> Arc<super::tuning::TaskHooks> {
    let mut hooks = inner.task_hooks.as_deref().cloned().unwrap_or_default();
    hooks.visiting = Some(Arc::new(super::sched::Visiting::new(inner.arc())));
    Arc::new(hooks)
}

/// Spawns `future` on `inner`'s worker `shard`.
#[inline(always)]
fn spawn_on<F>(inner: &Inner, shard: &Arc<Shared>, future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let hooks = inner.task_hooks.as_ref();
    let (runnable, task) = task::spawn(
        future,
        Sched::<false>::counted(Arc::clone(shard), None, hooks.cloned()),
    );
    spawned(hooks.map(|hooks| &**hooks), &task);
    // As its schedule would, without the turn through the task's table.
    if let Some(runnable) = super::sched::queue_here(shard, runnable, ScheduleInfo::new(false)) {
        runnable.schedule();
    }
    task
}

/// Calls the spawn hook, if the task's runtime has one, with the name a
/// `task::Builder` gives the task in place first, so every hook sees it.
#[inline(always)]
fn spawned<T>(hooks: Option<&super::tuning::TaskHooks>, task: &Task<T>) {
    if let Some(hooks) = hooks {
        spawned_with(hooks, task.id());
    }
}

/// [`spawned`] for a runtime with task hooks: out of line, so a spawn with
/// none carries none of it.
#[cold]
#[inline(never)]
fn spawned_with(hooks: &super::tuning::TaskHooks, id: task::Id) {
    crate::task::name_now(id);
    super::tuning::call(hooks.spawn.as_ref(), id);
}

/// The task hooks of the runtime set on this thread, looked for only while
/// some runtime of the process has any.
fn context_hooks() -> Option<Arc<super::tuning::TaskHooks>> {
    if !super::tuning::ANY_TASK_HOOKS.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    context::runtime().and_then(|inner| inner.task_hooks.clone())
}

/// Where a spawn goes on a thread that runs no shard: a task that is not bound, spawned from the
/// rest of a poll whose shard the monitor took, goes to that shard, whose runner runs it as it
/// would have before the take (proofs/tla/handoff/Detached.tla, `lost_spawn`); a bound task, or
/// a spawn from a thread that lost no shard, goes to the thread's own shard (answered `None`).
#[cold]
#[inline(never)]
fn lost_spawn(bound: bool) -> Option<Arc<Shared>> {
    if bound {
        return None;
    }
    context::lost_shared(|lost| lost.map(Arc::clone))
}

/// Spawns `future` on the shard of the calling thread: the worker it runs,
/// or the thread's own. `bound`: the future is not `Send` and must stay. A thread whose poll lost
/// its worker's shard to the monitor spawns a task that is not bound onto that shard.
fn spawn_here<F, const BOUND: bool>(
    future: F,
    hooks: Option<Arc<super::tuning::TaskHooks>>,
) -> Task<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let place = |shard: &Shard| {
        (
            Arc::clone(&shard.shared),
            BOUND.then(|| context::home_for(shard)),
        )
    };
    let (shared, home) =
        context::current(|current| current.map(place)).unwrap_or_else(|| match lost_spawn(BOUND) {
            Some(lost) => (lost, None),
            None => place(&own_shard()),
        });
    // One schedule type per spawn site, known at compile time: a future spawned bound has no
    // unbound task code made for it, and the other way round.
    let at = home.as_ref().map(Arc::as_ptr);
    let (runnable, task, bound_to) = if BOUND {
        // SAFETY: the task's schedule sends a woken bound task only to its
        // home's thread; the future is 'static.
        let (runnable, task) = unsafe {
            task::spawn_unchecked(future, Sched::<true>::counted(shared, home, hooks.clone()))
        };
        if let Some(at) = at {
            // SAFETY: `home_for` gives this thread's home, and the new task is on no list.
            unsafe { (*at).push_task(runnable.links()) };
        }
        (runnable, task, at)
    } else {
        let sched = match &hooks {
            Some(visitor) if visitor.visiting.is_some() => {
                Sched::<false>::visitor(shared, Arc::clone(visitor))
            }
            _ => Sched::<false>::counted(shared, None, hooks.clone()),
        };
        // SAFETY: the task's schedule sends a woken task of this shard back
        // to it; the future is 'static, and `Send` (a caller with `BOUND`
        // false is `spawn`'s, whose future is).
        let (runnable, task) = unsafe { task::spawn_unchecked(future, sched) };
        (runnable, task, None)
    };
    spawned(hooks.as_deref(), &task);
    // A bound task as its schedule would queue it, without the turn through
    // the task's table.
    let left = match bound_to {
        Some(home) => {
            // SAFETY: the task's schedule holds its home while the runnable
            // lives.
            let home = unsafe { &*home };
            super::sched::queue_home_here(home, runnable, ScheduleInfo::new(false))
        }
        None => Some(runnable),
    };
    if let Some(runnable) = left {
        runnable.schedule();
    }
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
    let runtime = context::set_runtime(inner);
    // Pinned here once: the calls below take it by reference.
    let mut future = core::pin::pin!(future);
    // On the shard this thread runs, the call nests, inside the shard: one of
    // another runtime than the shard serves keeps its timers in wheels of
    // its own and its loop runs under the one the shard serves
    // (`proofs/tla/nested/Nested.tla`, `proofs/tla/clock/OneWheel.tla`).
    if let Some(output) = context::current(|current| {
        current.map(|shard| shard.block_on_nested(future.as_mut(), runtime.previous(), inner))
    }) {
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
/// it run on its own. Awaiting a task that was cancelled (through an
/// [`AbortHandle`](crate::task::AbortHandle), or by its runtime's shutdown)
/// panics; [`Task::join`] and [`Task::fallible`] answer it instead.
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
    spawned.unwrap_or_else(spawn_current)
}

/// [`spawn`] on the runtime [`current`] finds: out of line, so the spawn of a
/// thread whose runtime is set and has no hooks carries none of it.
#[inline(never)]
fn spawn_current<F>(future: F) -> Task<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_with(&current(), future)
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
    spawn_here::<_, true>(future, context_hooks())
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
        task::spawn_unchecked(future, Sched::<false>::uncounted(shared))
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
/// A thread's CPU affinity mask: one bit per core of the process's group.
#[cfg(windows)]
pub(crate) type CpuMask = usize;
/// A thread's CPU affinity mask; none is kept where threads are not pinned.
#[cfg(not(any(target_os = "linux", windows)))]
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
    #[cfg(windows)]
    {
        use crate::syscore::windows::{GetCurrentThread, SetThreadAffinityMask};
        // SAFETY: the pseudo-handle of the calling thread, and a mask.
        unsafe { SetThreadAffinityMask(GetCurrentThread(), *mask) };
    }
    #[cfg(not(any(target_os = "linux", windows)))]
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
