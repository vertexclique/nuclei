//! A shard: one thread's run queue, timers and poller, and the half of it
//! other threads reach.
//!
//! [`Shared`] is what any thread may touch: the inbox other threads push
//! runnables into, the park handshake, the notifier that wakes the poller,
//! the registrar and mailbox of its IO registrations, and the count of live
//! tasks placement reads. [`Shard`] is the rest, owned by the thread that
//! runs it: the local run queue, the LIFO slot, the timer wheel, the poller,
//! the IO sources it watches and its buffer pool. Only that thread touches
//! it, so it is plain cells.
//!
//! # The run loop
//!
//! [`Shard::block_on`] polls one future and, between its polls, runs the
//! shard's tasks: the LIFO slot first (a task woken by the task that just
//! ran, at most [`LIFO_MAX`] times in a row), then the local queue, then the
//! inbox (first every [`INBOX_INTERVAL`] runs, so remote work is not
//! starved). Every [`DRIVER_INTERVAL`] runs, and every [`DRIVER_INTERVAL`]
//! turns in which only the future ran (it keeps waking itself), it fires
//! due timers and polls the poller without waiting, which completes the IO
//! of every source that became ready (see `io::source`); with the queues
//! empty it fires due timers each turn. With nothing to run and the future
//! not woken it spins briefly (see [`Spin`]), looking at the poller as it
//! does, then parks until the next timer, a remote push or a wake of the
//! future. Workers run the same loop on a future that ends at shutdown.
//!
//! # Nesting
//!
//! A task may call `block_on`: the nested loop runs on the same shard and
//! drives its other tasks, timers and poller meanwhile. The task that called
//! it is mid-poll, so a wake of it only marks it, and it runs again after
//! its poll returns: the loop never polls a task that is on the stack. The
//! depth is bounded by `max_nesting`; one level more panics with a message
//! that says so, before the thread's stack would overflow.

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kovan_queue::seg_queue::SegQueue;

use self::signal::Signal;
use super::backend::Backend;
use super::counters::{self, Counters};
use super::handshake::ParkState;
#[cfg(readiness)]
use super::park::Registrar;
use super::park::{Notifier, Poller};
use super::sources::Sources;
use super::task::{Runnable, ScheduleInfo};
use crate::buf::{self, Pool};
use crate::io::Source;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(ring)]
use crate::syscore::linux::iouring::ring::RingConfig;
use crate::time::TimerOp;
use crate::time::wheel::Wheel;
use spin::Spin;

mod children;
mod going;
#[cfg(port)]
mod port;
mod signal;
mod spin;
mod stats;
mod timers;
pub(crate) use children::{watch_exit, watch_exit_elsewhere};
#[cfg(ring)]
pub(crate) mod recv;
#[cfg(ring)]
pub(crate) mod ring;

/// Runs of the LIFO slot in a row before its task goes to the back.
pub(crate) const LIFO_MAX: u32 = 3;
/// The inbox is looked at first once every this many runs.
pub(crate) const INBOX_INTERVAL: u32 = 31;
/// Timers and the poller are looked at once every this many runs.
pub(crate) const DRIVER_INTERVAL: u32 = 61;
/// A shard that looks at its driver between tasks publishes its counts
/// once every this many looks; one that parks publishes every time.
pub(crate) const PUBLISH_INTERVAL: u32 = 8;

/// The part of a shard any thread may touch.
pub(crate) struct Shared {
    /// Runnables of `Send` tasks pushed by other threads.
    inbox: SegQueue<Runnable>,
    /// Runnables of tasks bound to this shard's thread, pushed by others.
    /// Kept apart: if the shard is gone, they are leaked rather than dropped
    /// on a thread their future may not be dropped on.
    local_inbox: SegQueue<Runnable>,
    park: ParkState,
    notifier: Notifier,
    /// Live tasks placed on the shard: what placement compares.
    pub(crate) load: AtomicUsize,
    /// The shard stopped: it takes no new task and runs no more.
    stopped: AtomicBool,
    /// The shard stops once its live tasks are done, and takes no new one.
    draining: AtomicBool,
    /// The waker of the worker's loop, woken by `stop`.
    stop_waker: AtomicWaker,
    /// Work on the timer wheel from other threads, applied by the shard
    /// before it fires timers.
    timer_ops: SegQueue<TimerOp>,
    /// The registrations of the shard's readiness poller; `None` on a ring
    /// shard, whose sources' IO is the ring's operations.
    #[cfg(readiness)]
    registrar: Option<Registrar>,
    /// The shard's completion port, which its sources' handles are bound
    /// to and their operations complete on.
    #[cfg(port)]
    port: Arc<super::park::iocp::Port>,
    /// Registrations other threads ask of the shard, applied before it
    /// polls.
    io_ops: SegQueue<IoOp>,
    /// What the shard counts, for `stats`.
    pub(crate) counters: Counters,
    /// While its runtime's clock is paused: the deadline of the shard's
    /// earliest timer when it last parked (nanoseconds after the clock's
    /// origin, `u64::MAX` for none).
    timer_at: AtomicU64,
    /// Parks the shard began: a paused clock tells a shard that slept
    /// throughout from one that woke and parked again.
    parks: AtomicU64,
}

/// A registration change another thread asks of a shard.
pub(crate) enum IoOp {
    /// Watch a source for a thread that runs no shard.
    Register(Arc<Source>),
    /// A source went (its kernel registration is already gone): free its
    /// slot.
    Release(u64),
    /// Put the file of a source into the ring's table: a copy of its
    /// descriptor, made while the source could not close, which the ring
    /// closes once the table holds the file.
    #[cfg(ring)]
    RingAttach(u64, std::os::fd::OwnedFd),
    /// A ring source went: cancel its sides' operations (by user data)
    /// and let go of its slot.
    #[cfg(ring)]
    RingDetach(u32, [Option<u64>; 2]),
    /// A ring source's side wants an operation submitted.
    #[cfg(ring)]
    RingKick(Arc<Source>),
    /// Cancel the ring operation of this user data.
    #[cfg(ring)]
    RingCancel(u64),
    /// Watch a child's exit for a thread that runs no shard.
    Watch(Arc<crate::process::exit::Exit>),
}

impl Shared {
    /// Hands a runnable to the shard from another thread and wakes it if it
    /// sleeps. `bound`: the task must run on the shard's thread only.
    pub(crate) fn push_remote(&self, runnable: Runnable, bound: bool) {
        if bound {
            self.local_inbox.push(runnable);
        } else {
            self.inbox.push(runnable);
            // A shard that stopped runs nothing more, and its last drain of
            // the inbox may be over: what is pushed then is dropped here (its
            // future may be dropped on any thread), or it would keep itself
            // and, through its schedule, the shard for good. The push and the
            // flag's store are both sequentially consistent: either the
            // drain finds it, or this sees the flag.
            if self.stopped.load(Ordering::SeqCst) {
                while let Ok(runnable) = self.inbox.try_pop() {
                    drop(runnable);
                }
                return;
            }
        }
        // The queue's push is sequentially consistent: the shard either finds
        // the runnable after it parks, or is parked now.
        self.park.wake(|| self.notifier.notify());
    }

    /// Leaves a wake on the shard's poller: a wait of it in progress ends,
    /// and one that has not begun yet ends as soon as it does.
    pub(crate) fn notify(&self) {
        self.notifier.notify();
    }

    /// Wakes the shard if it sleeps (after the caller made its reason
    /// visible with a sequentially consistent write).
    pub(crate) fn unpark(&self) {
        self.park.wake(|| self.notifier.notify());
    }

    /// Hands work on the timer wheel to the shard. An arm request wakes the
    /// shard, which may sleep past the new deadline otherwise.
    pub(crate) fn post_timer(&self, op: TimerOp) {
        let wake = op.needs_wake();
        self.timer_ops.push(op);
        if wake {
            self.unpark();
        }
    }

    /// Adds and removes the kernel registrations of the shard's readiness
    /// poller; `None` on a ring shard.
    #[cfg(readiness)]
    pub(crate) fn registrar(&self) -> Option<&Registrar> {
        self.registrar.as_ref()
    }

    /// The shard's completion port.
    #[cfg(port)]
    pub(crate) fn port(&self) -> &Arc<super::park::iocp::Port> {
        &self.port
    }

    /// Whether the shard's IO is the operations of a ring.
    #[cfg(ring)]
    pub(crate) fn is_ring(&self) -> bool {
        self.registrar.is_none()
    }

    /// The shard as other threads see it, as a line, for a test that failed.
    #[cfg(test)]
    pub(crate) fn state_line(&self) -> String {
        format!(
            "parked {}, inbox {}, local inbox {}, timer ops {}, io ops {}, load {}, stopped {}, draining {}",
            self.park.is_parked(),
            self.inbox.len(),
            self.local_inbox.len(),
            self.timer_ops.len(),
            self.io_ops.len(),
            self.load.load(Ordering::SeqCst),
            self.stopped.load(Ordering::SeqCst),
            self.draining.load(Ordering::SeqCst),
        )
    }

    /// Hands a registration change to the shard and wakes it.
    pub(crate) fn post_io(&self, op: IoOp) {
        self.io_ops.push(op);
        self.unpark();
    }

    /// Frees the slot of a source whose kernel registration is gone: at once
    /// on the shard's own thread, else through its mailbox.
    pub(crate) fn release_source(self: &Arc<Self>, token: u64) {
        let done = super::context::current(|current| {
            current.is_some_and(|shard| shard.is(self) && shard.try_release(token))
        });
        if !done {
            self.post_io(IoOp::Release(token));
        }
    }

    /// Whether the shard stopped.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Stops the shard: a worker's loop ends at its next look.
    pub(crate) fn stop(&self) {
        self.draining.store(true, Ordering::SeqCst);
        self.stopped.store(true, Ordering::SeqCst);
        self.stop_waker.wake();
        self.unpark();
    }

    /// Whether placement may give the shard a new task.
    pub(crate) fn takes_tasks(&self) -> bool {
        !self.draining.load(Ordering::Relaxed)
    }

    /// The shard takes no new task, and its worker exits once the live ones
    /// are done.
    pub(crate) fn drain(&self) {
        self.draining.store(true, Ordering::Relaxed);
        // Pairs with the fence of `last_task_done`: either this sees the
        // count at zero, or the last task sees the flag.
        crate::sync::internal::fence(Ordering::SeqCst);
        if self.load.load(Ordering::Relaxed) == 0 {
            self.stop_waker.wake();
        }
        self.unpark();
    }

    /// The shard's last live task is done: a draining worker may exit.
    pub(crate) fn last_task_done(&self) {
        crate::sync::internal::fence(Ordering::SeqCst);
        if self.draining.load(Ordering::Relaxed) {
            self.stop_waker.wake();
        }
    }

    fn may_exit(&self) -> bool {
        self.is_stopped()
            || (self.draining.load(Ordering::Acquire) && self.load.load(Ordering::Acquire) == 0)
    }

    /// Completes once the shard is stopped, or drained of its tasks: the
    /// future a worker's loop runs.
    pub(crate) fn stopped(&self) -> impl Future<Output = ()> + '_ {
        core::future::poll_fn(move |cx| {
            if self.may_exit() {
                return Poll::Ready(());
            }
            self.stop_waker.register(cx.waker());
            if self.may_exit() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }

    /// The shard's counters as they are now.
    pub(crate) fn stats(&self) -> crate::stats::ShardStats {
        self.counters.read(self.load.load(Ordering::Relaxed))
    }

    /// Parked with nothing pushed to it: idle, for a paused clock.
    pub(crate) fn idle(&self) -> bool {
        self.park.is_parked() && !self.has_remote_work()
    }

    pub(crate) fn timer_at(&self) -> u64 {
        self.timer_at.load(Ordering::SeqCst)
    }

    pub(crate) fn parks(&self) -> u64 {
        self.parks.load(Ordering::SeqCst)
    }

    fn has_remote_work(&self) -> bool {
        !self.inbox.is_empty()
            || !self.local_inbox.is_empty()
            || !self.timer_ops.is_empty()
            || !self.io_ops.is_empty()
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // A runnable of a thread-bound task pushed after its shard was gone:
        // its future may only be dropped on that thread, which has exited,
        // so the task is leaked instead.
        while let Ok(runnable) = self.local_inbox.try_pop() {
            core::mem::forget(runnable);
        }
    }
}

/// A shard's settings.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) max_nesting: usize,
    /// What the shard's IO buffers may hold, in bytes.
    pub(crate) buffer_cap: usize,
    /// What the shard drives its IO with.
    pub(crate) backend: Backend,
}

/// What a shard waits on and does its sources' IO with.
#[cfg_attr(
    ring,
    expect(clippy::large_enum_variant, reason = "one per shard, built with it")
)]
enum Driver {
    /// A readiness poller (epoll, kqueue): the driver makes the calls. On
    /// Windows, the completion port the sources' operations complete on.
    Poll(RefCell<Poller>),
    /// A ring: the sources' IO is its operations.
    #[cfg(ring)]
    Ring(ring::ShardRing),
}

/// The part of a shard its own thread owns.
pub(crate) struct Shard {
    pub(crate) shared: Arc<Shared>,
    queue: RefCell<VecDeque<Runnable>>,
    lifo: Cell<Option<Runnable>>,
    lifo_streak: Cell<u32>,
    tick: Cell<u32>,
    /// Runs since timers and the poller were last looked at.
    since_drive: Cell<u32>,
    /// Task polls run (the run loop's are added from `since_drive` as it
    /// starts again), and runnables taken from other threads: published for
    /// `stats` with the rest of the shard's cells.
    polls: Cell<u64>,
    /// Sleeps with nothing to run, and their time in all, for `stats`.
    sleeps: Cell<u64>,
    slept: Cell<Duration>,
    remote_wakes: Cell<u64>,
    /// Looks at the driver from the run loop.
    looks: Cell<u32>,
    spin: Spin,
    /// The runtime and the place this shard is a worker of, set when its
    /// worker thread starts: what `spawn` uses to keep a task local.
    owner: Cell<Option<(u64, usize)>>,
    /// A task is being polled: a wake it makes goes to the LIFO slot.
    in_task: Cell<bool>,
    depth: Cell<usize>,
    limits: Limits,
    tuning: super::tuning::Tuning,
    driver: Driver,
    wheel: RefCell<Wheel>,
    /// The wheel's zero: the clock's origin, the same for every shard, so a
    /// paused clock on the wheels' millisecond grid fires timers at their
    /// deadlines exactly.
    epoch: Instant,
    /// Wakers of fired timers, woken once the wheel is let go; reused.
    fired: RefCell<Vec<Waker>>,
    /// The IO sources the poller watches, by token.
    sources: RefCell<Sources>,
    /// The children whose exits the poller or the port watches, by token.
    children: RefCell<children::Children>,
    /// Where the shard's reads land and its writes wait.
    pool: Pool,
    /// The keeper of the shard's port while the shard does not run.
    #[cfg(port)]
    keeper: RefCell<Option<Arc<port::Keeper>>>,
}

/// The fewest submission entries a ring short of memory is made with.
#[cfg(ring)]
const MIN_RING_ENTRIES: u32 = 64;

impl Shard {
    pub(crate) fn new(limits: Limits) -> io::Result<Self> {
        Self::made(limits, None)
    }

    /// The shard of worker `index` of runtime `runtime`, run as `tuning`
    /// says.
    pub(crate) fn worker(
        limits: Limits,
        tuning: super::tuning::Tuning,
        runtime: u64,
        index: usize,
    ) -> io::Result<Self> {
        let mut shard = Self::made(limits, Some((runtime, index)))?;
        shard.tuning = tuning;
        Ok(shard)
    }

    /// A shard, worker `owner` (a runtime and a place) if it is one. Its
    /// counters know the place before `stats` can list it.
    fn made(limits: Limits, owner: Option<(u64, usize)>) -> io::Result<Self> {
        #[cfg(readiness)]
        let poller = || -> io::Result<_> {
            let poller = Poller::new()?;
            let (notifier, registrar) = (poller.notifier(), poller.registrar());
            Ok((
                Driver::Poll(RefCell::new(poller)),
                notifier,
                Some(registrar),
            ))
        };
        #[cfg(port)]
        let poller = || -> io::Result<_> {
            let poller = Poller::new()?;
            let (notifier, port) = (poller.notifier(), Arc::clone(poller.port()));
            Ok((Driver::Poll(RefCell::new(poller)), notifier, port))
        };
        // The poller's registrar, or the port its handles bind to.
        let (driver, notifier, kernel) = match limits.backend {
            Backend::Poll => poller()?,
            #[cfg(ring)]
            Backend::Ring(cfg) | Backend::RingOrPoll(cfg) => {
                // Under a tight memory limit the ring gets fewer entries; one
                // `auto` chose runs the poller if even the least finds none.
                let made = super::backend::with_room(cfg.entries, MIN_RING_ENTRIES, |entries| {
                    ring::ShardRing::new(&RingConfig { entries, ..cfg }, ring::table_len())
                });
                match made {
                    Ok(ring) => {
                        let notifier = ring.notifier();
                        (Driver::Ring(ring), notifier, None)
                    }
                    Err(err)
                        if matches!(limits.backend, Backend::RingOrPoll(_))
                            && err.raw_os_error() == Some(libc::ENOMEM) =>
                    {
                        poller()?
                    }
                    Err(err) => return Err(err),
                }
            }
        };
        let pool = Pool::new(limits.buffer_cap);
        let shared = Arc::new(Shared {
            inbox: SegQueue::new(),
            local_inbox: SegQueue::new(),
            park: ParkState::new(),
            notifier,
            load: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            stop_waker: AtomicWaker::new(),
            timer_ops: SegQueue::new(),
            #[cfg(readiness)]
            registrar: kernel,
            #[cfg(port)]
            port: kernel,
            io_ops: SegQueue::new(),
            counters: Counters::new(driver.backend(), pool.cap(), owner.map(|(_, index)| index)),
            timer_at: AtomicU64::new(u64::MAX),
            parks: AtomicU64::new(0),
        });
        counters::register(&shared);
        Ok(Self {
            shared,
            queue: RefCell::new(VecDeque::new()),
            lifo: Cell::new(None),
            lifo_streak: Cell::new(0),
            tick: Cell::new(0),
            since_drive: Cell::new(0),
            polls: Cell::new(0),
            sleeps: Cell::new(0),
            slept: Cell::new(Duration::ZERO),
            remote_wakes: Cell::new(0),
            looks: Cell::new(0),
            spin: Spin::new(),
            owner: Cell::new(owner),
            in_task: Cell::new(false),
            depth: Cell::new(0),
            limits,
            tuning: super::tuning::Tuning::default(),
            driver,
            wheel: RefCell::new(Wheel::new()),
            epoch: crate::time::clock::origin(),
            fired: RefCell::new(Vec::new()),
            sources: RefCell::new(Sources::default()),
            children: RefCell::new(children::Children::default()),
            pool,
            #[cfg(port)]
            keeper: RefCell::new(None),
        })
    }

    /// Watches `source` from now on, under a new token. The caller moved
    /// the source's registration to registering.
    pub(crate) fn register_source(&self, source: &Arc<Source>) -> io::Result<()> {
        let token = match self.sources.borrow_mut().insert(Arc::clone(source)) {
            Ok(token) => token,
            Err(err) => {
                self.shared.counters.refused_registration();
                source.registration_abandoned();
                return Err(err);
            }
        };
        // A failed attach hands the slot back itself; one left to another
        // thread reports its failure to the tasks waiting on the source.
        match source.attach(&self.shared, token) {
            Some(Err(err)) => Err(err),
            _ => Ok(()),
        }
    }

    /// Frees a released source's slot, unless the slots are in use (the
    /// release then goes through the mailbox).
    fn try_release(&self, token: u64) -> bool {
        let Ok(mut sources) = self.sources.try_borrow_mut() else {
            return false;
        };
        let gone = sources.remove(token);
        drop(sources);
        drop(gone);
        true
    }

    /// Applies the registration changes other threads asked for.
    fn apply_io_ops(&self) {
        while let Ok(op) = self.shared.io_ops.try_pop() {
            match op {
                IoOp::Register(source) => {
                    if let Err(err) = self.register_source(&source) {
                        source.registration_failed(&err);
                    }
                }
                IoOp::Release(token) => {
                    let gone = self.sources.borrow_mut().remove(token);
                    drop(gone);
                }
                IoOp::Watch(exit) => self.watch(exit),
                #[cfg(ring)]
                IoOp::RingAttach(token, copy) => {
                    let source = self.sources.borrow().get(token).cloned();
                    if let (Some(ring), Some(source)) = (self.ring(), source) {
                        source.ring_attached(ring, &self.shared, &copy, token);
                    }
                    drop(copy);
                }
                #[cfg(ring)]
                IoOp::RingDetach(slot, out) => {
                    if let Some(ring) = self.ring() {
                        ring.detach(slot, out);
                    }
                }
                #[cfg(ring)]
                IoOp::RingKick(source) => source.ring_kick(),
                #[cfg(ring)]
                IoOp::RingCancel(user_data) => {
                    if let Some(ring) = self.ring() {
                        ring.cancel(user_data);
                    }
                }
            }
        }
    }

    /// The address of the shard's `Shared`, which a source keeps to tell
    /// the shard it is registered with: compared, never followed.
    pub(crate) fn shared_addr(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }

    /// Whether `shared` is this shard's.
    pub(crate) fn is(&self, shared: &Arc<Shared>) -> bool {
        Arc::ptr_eq(&self.shared, shared)
    }

    /// This shard's place among the workers of runtime `runtime`, if it is
    /// one of them.
    pub(crate) fn worker_index(&self, runtime: u64) -> Option<usize> {
        self.owner
            .get()
            .and_then(|(owner, index)| (owner == runtime).then_some(index))
    }

    /// Queues a runnable from this shard's own thread. A task woken by the
    /// task being polled runs next (the LIFO slot), unless it is that task
    /// itself yielding.
    pub(crate) fn push_local(&self, runnable: Runnable, info: ScheduleInfo) {
        if self.in_task.get() && !info.woken_while_running {
            if let Some(previous) = self.lifo.replace(Some(runnable)) {
                self.queue.borrow_mut().push_back(previous);
            }
        } else {
            self.queue.borrow_mut().push_back(runnable);
        }
    }

    /// The next runnable, in the loop's order.
    fn next(&self) -> Option<Runnable> {
        let tick = self.tick.get().wrapping_add(1);
        self.tick.set(tick);
        if tick.is_multiple_of(self.tuning.inbox_interval)
            && let Some(r) = self.pop_inbox()
        {
            return Some(r);
        }
        if let Some(r) = self.lifo.take() {
            if self.lifo_streak.get() < LIFO_MAX {
                self.lifo_streak.set(self.lifo_streak.get() + 1);
                return Some(r);
            }
            // The slot's task ran enough times in a row: to the back.
            self.queue.borrow_mut().push_back(r);
        }
        self.lifo_streak.set(0);
        let local = self.queue.borrow_mut().pop_front();
        local.or_else(|| self.pop_inbox())
    }

    fn pop_inbox(&self) -> Option<Runnable> {
        let runnable = self
            .shared
            .inbox
            .try_pop()
            .or_else(|_| self.shared.local_inbox.try_pop())
            .ok()?;
        self.remote_wakes.set(self.remote_wakes.get() + 1);
        Some(runnable)
    }

    /// Runnables queued on this thread now, the LIFO slot's included.
    fn queued(&self) -> usize {
        let lifo = self.lifo.take();
        let n = self.queue.borrow().len() + usize::from(lifo.is_some());
        self.lifo.set(lifo);
        n
    }

    fn has_local_work(&self) -> bool {
        // The LIFO slot is looked at by taking it and putting it back.
        let lifo = self.lifo.take();
        let busy = lifo.is_some() || !self.queue.borrow().is_empty();
        self.lifo.set(lifo);
        busy
    }

    /// Polls a runnable's task, marking that a task runs.
    fn run(&self, runnable: Runnable) {
        let outer = self.in_task.replace(true);
        runnable.run();
        self.in_task.set(outer);
    }

    /// Applies registration changes and fires due timers, then looks at the
    /// poller (waiting up to `wait`, or until the next timer) and completes
    /// the IO of the sources that became ready.
    fn drive(&self, wait: Option<Duration>) -> io::Result<()> {
        // The run loop's look between tasks, or its park (which may not wait
        // after all, its timer due): a park publishes the counts always, the
        // shard about to go quiet, and a look once every few.
        let looks = self.looks.get().wrapping_add(1);
        self.looks.set(looks);
        let publish = wait != Some(Duration::ZERO) || looks.is_multiple_of(PUBLISH_INTERVAL);
        if !self.shared.io_ops.is_empty() {
            self.apply_io_ops();
        }
        self.fire_timers();
        // A timer that fired just now (or a change applied above) may have
        // woken a task of this shard, which nothing else will wake it for:
        // the poller is only looked at, and the task runs at once.
        let wait = if self.has_local_work() {
            Some(Duration::ZERO)
        } else {
            wait
        };
        let timeout = match (wait, self.timer_wait(wait.is_none())) {
            (Some(w), Some(t)) => Some(w.min(t)),
            (w, t) => w.or(t),
        };
        if publish {
            self.publish();
        }
        self.poll_io(self.tuning.max_io_events, timeout)?;
        self.fire_timers();
        Ok(())
    }

    /// Waits on the poller up to `timeout` (`None` for no bound) and
    /// completes the IO of the sources that became ready, taking at most
    /// `max` events from the kernel. Returns how many it handled.
    fn poll_io(&self, max: usize, timeout: Option<Duration>) -> io::Result<usize> {
        match &self.driver {
            #[cfg(readiness)]
            Driver::Poll(poller) => {
                let sources = self.sources.borrow();
                let mut events = 0;
                poller
                    .borrow_mut()
                    .wait_at_most(max, timeout, |token, readiness| {
                        events += 1;
                        if token & crate::rt::sources::CHILD != 0 {
                            self.exit_came(token);
                        } else if let Some(source) = sources.get(token) {
                            source.on_event(token, readiness);
                        }
                    })?;
                Ok(events)
            }
            #[cfg(port)]
            Driver::Poll(poller) => {
                let mut events = 0;
                poller.borrow_mut().wait_at_most(max, timeout, |entry| {
                    events += 1;
                    if entry.lpCompletionKey == crate::rt::park::iocp::EXIT {
                        // A child's exit: its packet carries the child's token.
                        self.exit_came(entry.lpOverlapped as usize as u64);
                        return;
                    }
                    // SAFETY: a packet of a source's key is one of its
                    // operations, its OVERLAPPED the first field of a
                    // slot the operation's strong reference keeps.
                    unsafe { crate::io::source::port::complete(entry) };
                })?;
                Ok(events)
            }
            #[cfg(ring)]
            Driver::Ring(ring) => ring.wait(timeout).map(|(handled, _)| handled),
        }
    }

    /// The kernel interface this shard runs its IO on.
    pub(crate) fn io_backend(&self) -> crate::IoBackend {
        self.driver.backend()
    }

    /// The shard's ring, if its IO runs on one.
    #[cfg(ring)]
    pub(crate) fn ring(&self) -> Option<&ring::ShardRing> {
        match &self.driver {
            Driver::Ring(ring) => Some(ring),
            Driver::Poll(_) => None,
        }
    }

    /// Completes the IO that is ready now, without waiting: whether there
    /// was any.
    fn io_ready_now(&self) -> bool {
        match self.poll_io(usize::MAX, Some(Duration::ZERO)) {
            Ok(events) => events > 0,
            Err(err) => panic!("the shard's poller failed: {err}"),
        }
    }

    /// Runs `future` to its end on this shard, running the shard's tasks,
    /// timers and poller between its polls.
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        let depth = self.depth.get();
        if depth >= self.limits.max_nesting {
            panic!(
                "block_on is nested {} deep on one thread, the most allowed: each level runs on \
                 the thread's stack (raise the limit with `Builder::max_nesting`)",
                self.limits.max_nesting
            );
        }
        struct Depth<'a>(&'a Cell<usize>);
        impl Drop for Depth<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() - 1);
            }
        }
        self.depth.set(depth + 1);
        let _depth = Depth(&self.depth);
        let _pool = buf::enter(&self.pool);

        let signal = Arc::new(Signal {
            woken: AtomicBool::new(true),
            shared: Arc::clone(&self.shared),
        });
        let waker = Waker::from(Arc::clone(&signal));
        let mut cx = Context::from_waker(&waker);
        let mut future = pin!(future);
        // Turns in which the future woke itself and no task ran.
        let mut self_woken = 0u32;
        // Tasks to run before the future's next poll: a future that woke
        // itself (a yield) lets the tasks queued at that moment run first,
        // one pass of the queue.
        let mut owed = 0usize;
        loop {
            if owed == 0
                && signal.woken.load(Ordering::Relaxed)
                && signal.woken.swap(false, Ordering::SeqCst)
            {
                let outer = self.in_task.replace(true);
                let budget = crate::coop::enter();
                let poll = future.as_mut().poll(&mut cx);
                crate::coop::leave(budget);
                self.in_task.set(outer);
                if let Poll::Ready(output) = poll {
                    return output;
                }
                if signal.woken.load(Ordering::Relaxed) {
                    owed = self.queued();
                }
            }
            if let Some(runnable) = self.next() {
                owed = owed.saturating_sub(1);
                self.run(runnable);
                let since = self.since_drive.get() + 1;
                if since >= self.tuning.driver_interval {
                    // Timers and the poller get a look, without a wait.
                    self.count_again(since);
                    self.drive_or_report(Some(Duration::ZERO));
                } else {
                    self.since_drive.set(since);
                }
                continue;
            }
            // The queue ran dry: nothing is owed any more.
            owed = 0;
            // Nothing to run: due timers may have made work, and work other
            // threads posted (which the spin and the park count as work) is
            // applied here, so it never keeps the loop from sleeping.
            if !self.shared.io_ops.is_empty() {
                self.apply_io_ops();
            }
            self.fire_timers();
            if self.has_local_work() || signal.woken.load(Ordering::SeqCst) {
                // The future woke itself again, or a task is ready: the
                // driver gets a look every `DRIVER_INTERVAL` such turns too,
                // or IO the future waits on (on a ring, an operation not even
                // submitted yet) would never complete.
                self_woken += 1;
                if self_woken >= self.tuning.driver_interval {
                    self_woken = 0;
                    self.count_again(self.since_drive.get());
                    self.drive_or_report(Some(Duration::ZERO));
                }
                continue;
            }
            self.count_again(self.since_drive.get());
            let shared = &self.shared;
            // The spin looks at the poller too: IO that comes within it
            // costs neither a park nor the kernel's wake of this thread.
            if self.spin.run(|| {
                shared.has_remote_work()
                    || signal.woken.load(Ordering::Relaxed)
                    || self.io_ready_now()
            }) {
                continue;
            }
            self.before_park();
            if shared
                .park
                .begin_park(|| shared.has_remote_work() || signal.woken.load(Ordering::SeqCst))
            {
                let slept = Instant::now();
                if let Some(park) = &self.tuning.hooks.park {
                    park();
                }
                self.drive_or_report(None);
                shared.park.end_park();
                if let Some(unpark) = &self.tuning.hooks.unpark {
                    unpark();
                }
                self.slept_for(slept.elapsed());
            }
            self.after_park();
        }
    }

    /// One turn of the shard, for a caller driving it by hand: runs every
    /// task ready now, waits once on the poller (up to `wait`, at once if
    /// work is ready; at most `max` events) and runs what the IO woke.
    /// Returns how many IO events it handled.
    pub(crate) fn turn(&self, max: usize, wait: Option<Duration>) -> io::Result<usize> {
        let _pool = buf::enter(&self.pool);
        self.run_ready();
        if !self.shared.io_ops.is_empty() {
            self.apply_io_ops();
        }
        self.fire_timers();
        let shared = &self.shared;
        let quiet = wait != Some(Duration::ZERO) && !self.has_local_work();
        if quiet {
            self.before_park();
        }
        let sleeps = quiet && shared.park.begin_park(|| shared.has_remote_work());
        let wait = if sleeps || wait == Some(Duration::ZERO) {
            wait
        } else {
            Some(Duration::ZERO)
        };
        let timeout = match (wait, self.timer_wait(sleeps)) {
            (Some(w), Some(t)) => Some(w.min(t)),
            (w, t) => w.or(t),
        };
        self.publish();
        let result = self.poll_io(max, timeout);
        if sleeps {
            shared.park.end_park();
        }
        if quiet {
            self.after_park();
        }
        let events = result?;
        self.fire_timers();
        self.run_ready();
        Ok(events)
    }

    /// Runs every task that is ready now, and the ones they wake.
    fn run_ready(&self) {
        while let Some(runnable) = self.next() {
            self.run(runnable);
            self.polls.set(self.polls.get() + 1);
        }
    }

    /// Starts the count of runs before the next look at the driver again,
    /// adding the `runs` it held to the polls: the run loop counts its
    /// polls there, never one by one.
    pub(super) fn count_again(&self, runs: u32) {
        self.polls.set(self.polls.get() + u64::from(runs));
        self.since_drive.set(0);
    }

    /// Drives the poller; a failure of the poller itself cannot be handed to
    /// any task, so it ends the shard loudly.
    fn drive_or_report(&self, wait: Option<Duration>) {
        if let Err(err) = self.drive(wait) {
            panic!("the shard's poller failed: {err}");
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
