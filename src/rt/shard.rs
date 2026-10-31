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
//! due timers and polls the poller without waiting, as does every
//! [`QUARTZ_EVERY`](super::tuning::QUARTZ_EVERY)th run once the coarse clock says
//! [`DRIVE_BOUND`](super::tuning::DRIVE_BOUND) has passed since the last look, so a
//! few long runs hold the driver off no longer than many short ones; a runner
//! that takes the shard over looks before it runs a task. The look completes
//! the IO of every source that became ready (see `io::source`); with the queues
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
use core::task::{Poll, Waker};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::inbox::{Inbox, Pop};
use kovan_queue::seg_queue::SegQueue;

use super::backend::Backend;
use super::counters::{self, Counters};
use super::handshake::ParkState;
#[cfg(readiness)]
use super::park::Registrar;
use super::park::{Notifier, Poller};
use super::sources::Sources;
use super::task::{Runnable, ScheduleInfo};
use crate::buf::Pool;
use crate::io::Source;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use crate::time::TimerOp;
use crate::time::wheel::Wheel;
use spin::Spin;

mod children;
mod going;
#[cfg(port)]
mod port;
mod run;
pub(crate) mod signal;
mod spin;
mod stats;
mod timers;
pub(crate) use children::{watch_exit, watch_exit_elsewhere};
pub(crate) use run::Worked;
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
    /// Runnables of `Send` tasks pushed by other threads, linked through
    /// the tasks (see `rt::inbox`). Its taker is the shard, and once the
    /// shard stopped whoever drains it.
    inbox: Inbox,
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
    /// Nothing will apply `io_ops` again (the shard went, and its ring's
    /// keeper too): what reaches it then is let go of by its poster.
    io_closed: AtomicBool,
    /// What the shard counts, for `stats`.
    pub(crate) counters: Counters,
    /// A worker's shard: its runtime, inside which a task of the shard woken
    /// on another thread after the shard stopped is dropped.
    runtime: std::sync::OnceLock<std::sync::Weak<super::runtime::Inner>>,
    /// File writes issued on the shard whose end's wake has not gone yet.
    writes: AtomicUsize,
    /// The shard drained its inbox for the last time as it was dropped, and
    /// let go of its taker's role: a task woken here from now on is dropped
    /// by the thread that pushed it, or one that drains for it.
    released: AtomicBool,
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
    /// Put the file of a source into the ring's table (or a slot naming its
    /// descriptor): a lease on the handle's object, taken while the source
    /// could not close, which keeps the number the handle's until the ring
    /// took it.
    #[cfg(ring)]
    RingAttach(u64, crate::io::source::requests::Lease),
    /// A ring source went: cancel its sides' operations (by user data),
    /// hand every entry naming its slot to the kernel and take its file out
    /// of it, then let go of the lease that kept the object open meanwhile.
    #[cfg(ring)]
    RingDetach(
        u32,
        [Option<u64>; 2],
        Option<crate::io::source::requests::Lease>,
    ),
    /// A ring source's side wants an operation submitted.
    #[cfg(ring)]
    RingKick(Arc<Source>),
    /// Cancel the ring operation of this user data.
    #[cfg(ring)]
    RingCancel(u64),
    /// A thread whose heavy fence failed left work on a source's sides: the
    /// owner takes and lets go of them, which serves it.
    #[cfg(not(ring))]
    Touch(Arc<Source>),
    /// Watch a child's exit for a thread that runs no shard.
    Watch(Arc<crate::process::exit::Exit>),
}

impl IoOp {
    /// The op's shard went and applies it never: a registration asked of it
    /// fails, for the next wait to make it elsewhere, and a child it was to
    /// watch is watched where its user polls next; the rest only goes.
    fn gone(self) {
        match self {
            IoOp::Register(source) => source.registration_failed(&io::Error::new(
                io::ErrorKind::NotConnected,
                "the shard stopped before the registration",
            )),
            IoOp::Watch(exit) => {
                let orphan = Arc::strong_count(&exit) == 1;
                exit.unwatched(orphan);
            }
            other => drop(other),
        }
    }
}

impl Shared {
    /// Hands a runnable of a `Send` task to the shard from another thread
    /// and wakes it if it sleeps.
    pub(crate) fn push_remote(&self, runnable: Runnable) {
        self.inbox.push(runnable);
        // A shard that stopped runs nothing more, and its last drain of the
        // inbox may be over: what is pushed then is dropped (its future may
        // be dropped on any thread), or it would keep itself and, through its
        // schedule, the shard for good. The push and the flag's store are
        // both sequentially consistent: either the drain finds it, or this
        // sees the flag.
        if self.stopped.load(Ordering::SeqCst) {
            self.drop_stopped(false);
            return;
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
    /// shard, which may sleep past the new deadline otherwise. A shard that
    /// stopped applies nothing more: what reaches it then is drained here.
    pub(crate) fn post_timer(&self, op: TimerOp) {
        let wake = op.needs_wake();
        self.timer_ops.push(op);
        // Pairs with the fence of `drain_timer_ops`: either this sees the
        // stop, or the shard's last drain finds the op.
        crate::sync::internal::fence(Ordering::SeqCst);
        if self.stopped.load(Ordering::SeqCst) {
            self.drain_timer_ops();
            return;
        }
        if wake {
            self.unpark();
        }
    }

    /// Drains the timer work of a shard that stopped and fires no timer
    /// again: an arm request's timer is orphaned, and its sleep arms it again
    /// where it is polled (proofs/tla/timer, `S_go`); the rest only goes.
    pub(crate) fn drain_timer_ops(&self) {
        crate::sync::internal::fence(Ordering::SeqCst);
        while let Ok(op) = self.timer_ops.try_pop() {
            op.orphan();
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
            "parked {}, inbox {}, timer ops {}, io ops {}, load {}, stopped {}, draining {}",
            self.park.is_parked(),
            if self.inbox.is_empty() {
                "empty"
            } else {
                "not empty"
            },
            self.timer_ops.len(),
            self.io_ops.len(),
            self.load.load(Ordering::SeqCst),
            self.stopped.load(Ordering::SeqCst),
            self.draining.load(Ordering::SeqCst),
        )
    }

    /// Hands a registration change to the shard and wakes it. One that
    /// nothing will apply (the shard and its ring's keeper went) is let go
    /// of here, as the shard's last look does.
    pub(crate) fn post_io(&self, op: IoOp) {
        self.io_ops.push(op);
        // Pairs with the fence of `close_io`: either this sees the mailbox
        // closed, or its last drain finds the op.
        crate::sync::internal::fence(Ordering::SeqCst);
        if self.io_closed.load(Ordering::SeqCst) {
            self.drain_io_ops();
            return;
        }
        self.unpark();
    }

    /// Nothing applies the mailbox from now on: what it holds is let go of,
    /// and so is what reaches it later. The ring it was for is gone, with
    /// every entry it had, so a lease a detach carries may close its object.
    pub(crate) fn close_io(&self) {
        self.io_closed.store(true, Ordering::SeqCst);
        self.drain_io_ops();
    }

    fn drain_io_ops(&self) {
        crate::sync::internal::fence(Ordering::SeqCst);
        while let Ok(op) = self.io_ops.try_pop() {
            op.gone();
        }
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

    /// Whether the shard sleeps in its park now.
    pub(crate) fn is_parked(&self) -> bool {
        self.park.is_parked()
    }

    /// Whether the shard stopped.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Makes `runtime` the one this worker's shard belongs to.
    pub(crate) fn set_runtime(&self, runtime: std::sync::Weak<super::runtime::Inner>) {
        let _ = self.runtime.set(runtime);
    }

    /// Sets the shard's runtime on this thread, unless it is set already:
    /// for a task of the shard dropped where its waker ran (on another
    /// thread, the shard stopped), so what its drop hands on (a dropped
    /// handle's bytes) goes to its runtime (`rt::linger`).
    pub(crate) fn enter_runtime(&self) -> Option<super::context::RuntimeSet> {
        let runtime = self.runtime.get()?;
        if super::context::runtime_is(runtime.as_ptr()) {
            return None;
        }
        runtime
            .upgrade()
            .and_then(|inner| super::context::try_set_runtime(&inner))
    }

    /// A file write was issued on the shard.
    pub(crate) fn write_began(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
    }

    /// A file write issued here ended and its wake went: a runtime's drop
    /// that waits for the stopped shard looks again.
    pub(crate) fn write_ended(&self) {
        if self.writes.fetch_sub(1, Ordering::SeqCst) == 1 && self.stopped.load(Ordering::SeqCst) {
            self.nudge_runtime();
        }
    }

    /// What a runtime's drop waits for of its worker's shard: every file
    /// write issued here ended with its wake gone, and, once the shard let
    /// go of its inbox, every task woken since was dropped (the inbox is
    /// empty and nobody drains it). Read in this order, the inbox after the
    /// writes, so a task a write's wake left to another thread's drain is
    /// seen. A shard not let go of yet (its thread blocked in a task's poll,
    /// the shard handed on) drops what its inbox holds when that thread
    /// lets go of it, which the drop does not wait for.
    pub(crate) fn settled(&self) -> bool {
        self.writes.load(Ordering::SeqCst) == 0
            && (!self.released.load(Ordering::SeqCst) || self.inbox.is_idle())
    }

    /// The shard let go of its inbox for good (see [`settled`](Self::settled)).
    pub(crate) fn release_inbox(&self) {
        self.drop_stopped(true);
        self.released.store(true, Ordering::SeqCst);
    }

    /// Wakes the drop of the shard's runtime, if it waits.
    fn nudge_runtime(&self) {
        if let Some(inner) = self.runtime.get().and_then(std::sync::Weak::upgrade) {
            inner.lingering().nudge();
        }
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

    /// Drops what the inbox of a stopped shard holds, while this thread
    /// holds the inbox's taker's role (`holding`: it holds it already).
    /// Each drainer looks again after it lets go, and takes the role back
    /// if something came: a push that found the role held is not left
    /// (proofs/tla/park, `ShardStop`).
    pub(crate) fn drop_stopped(&self, mut holding: bool) {
        loop {
            if !holding && !self.inbox.try_take() {
                return;
            }
            // SAFETY: the role is this thread's.
            unsafe {
                self.inbox.drain(drop);
                self.inbox.release();
            }
            holding = false;
            if self.inbox.is_empty() {
                break;
            }
        }
        // A runtime's drop waits until no task woken here is left to drop.
        self.nudge_runtime();
    }

    /// Whether work was handed to the shard, as any thread sees it (for a
    /// paused clock deciding whether the shard is idle); its own thread looks
    /// with [`Shard::has_remote_work`].
    fn has_remote_work(&self) -> bool {
        !self.inbox.is_empty() || !self.timer_ops.is_empty() || !self.io_ops.is_empty()
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

/// Charges `pool` with the receive group the ring of `backend` asks for, and
/// returns its bytes (0 off a ring, or with no group); refuses a group that
/// leaves the pool no room for a buffer of its own.
fn reserve_group(pool: &Pool, backend: Backend) -> io::Result<usize> {
    #[cfg(ring)]
    if let Backend::Ring(cfg) | Backend::RingOrPoll(cfg) = backend {
        let bytes = recv::reservation(cfg.recv_buffers, cfg.recv_buffer_size);
        if !pool.reserve(bytes) {
            return Err(recv::too_large(
                cfg.recv_buffers,
                cfg.recv_buffer_size,
                bytes,
                pool.cap(),
            ));
        }
        return Ok(bytes);
    }
    let _ = (pool, backend);
    Ok(0)
}

/// The least `io_buffer_cap` a runtime of the process's backend starts
/// with: the receive group of each shard, and one buffer of the pool's
/// smallest class.
#[cfg(test)]
pub(crate) fn least_buffer_cap() -> usize {
    let group = match super::backend::chosen() {
        #[cfg(ring)]
        Ok(Backend::Ring(cfg) | Backend::RingOrPoll(cfg)) => {
            recv::reservation(cfg.recv_buffers, cfg.recv_buffer_size)
        }
        _ => 0,
    };
    group + crate::buf::CLASSES[0]
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
    /// The ring went to a keeper as the shard went (`ring::keeper`): only the shard's drop sees
    /// this.
    #[cfg(ring)]
    Gone,
}

/// The part of a shard its own thread owns.
pub(crate) struct Shard {
    pub(crate) shared: Arc<Shared>,
    queue: RefCell<VecDeque<Runnable>>,
    lifo: Cell<Option<Runnable>>,
    lifo_streak: Cell<u32>,
    /// Runs left before the inbox is looked at first (`tuning.inbox_interval` at the start).
    until_inbox: Cell<u32>,
    /// Runs since timers and the poller were last looked at.
    since_drive: Cell<u32>,
    /// The `Quartz` reading of the last look at timers and the poller.
    last_drive: Cell<u64>,
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
    /// The monitor of the shard's runtime, for a worker shard it watches:
    /// told when the shard's park ends.
    monitor: Option<Arc<super::handoff::Monitor>>,
    /// A thread's own shard: the home of the bound tasks spawned on it,
    /// made on the first, and dropped with the shard (so a task woken as the
    /// shard goes is dropped on its thread).
    home: core::cell::OnceCell<Arc<super::handoff::home::Home>>,
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
        monitor: Option<Arc<super::handoff::Monitor>>,
    ) -> io::Result<Self> {
        let mut shard = Self::made(limits, Some((runtime, index)))?;
        shard.until_inbox.set(tuning.inbox_interval);
        #[cfg(ring)]
        if let Some(ring) = shard.ring() {
            ring.set_zero_copy_min(tuning.zero_copy_min);
        }
        shard.tuning = tuning;
        shard.monitor = monitor;
        Ok(shard)
    }

    /// The shard's buffer pool.
    pub(crate) fn pool(&self) -> &Pool {
        &self.pool
    }

    /// The home of the bound tasks spawned on this shard, a thread's own,
    /// made now if it has none.
    pub(crate) fn own_home(&self) -> Arc<super::handoff::home::Home> {
        Arc::clone(
            self.home.get_or_init(|| {
                Arc::new(super::handoff::home::Home::new(Arc::clone(&self.shared)))
            }),
        )
    }

    /// Whether `home` is this shard's own.
    pub(crate) fn owns_home(&self, home: &super::handoff::home::Home) -> bool {
        self.home
            .get()
            .is_some_and(|own| core::ptr::eq(Arc::as_ptr(own), home))
    }

    /// The shard's park ended: its runtime's monitor, asleep while every
    /// worker was parked, wakes.
    fn parked_ended(&self) {
        if let Some(monitor) = &self.monitor {
            monitor.worker_woke();
        }
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
        let pool = Pool::new(limits.buffer_cap);
        // The receive group asked for is charged before it is made; what the
        // ring made in the end is less on a ring made smaller, and nothing on
        // the poller, and the rest is given back below.
        let reserved = reserve_group(&pool, limits.backend)?;
        // The poller's registrar, or the port its handles bind to.
        let (driver, notifier, kernel) = match limits.backend {
            Backend::Poll => poller()?,
            #[cfg(ring)]
            Backend::Ring(cfg) | Backend::RingOrPoll(cfg) => {
                // Under a tight memory limit the ring gets fewer entries; one
                // `auto` chose runs the poller if even the least finds none.
                match ring::ShardRing::for_shard(cfg) {
                    Ok(ring) => {
                        let notifier = ring.notifier();
                        (Driver::Ring(ring), notifier, None)
                    }
                    // The choice of `auto` checked the kernel already (see
                    // `backend::usable`); a shard that still finds the ring
                    // refused, or no memory, runs the poller rather than fail.
                    Err(err)
                        if matches!(limits.backend, Backend::RingOrPoll(_))
                            && (err.raw_os_error() == Some(libc::ENOMEM)
                                || err.kind() == io::ErrorKind::Unsupported) =>
                    {
                        poller()?
                    }
                    Err(err) => return Err(err),
                }
            }
        };
        let made = match &driver {
            #[cfg(ring)]
            Driver::Ring(ring) => ring.recv_reservation(),
            #[cfg(ring)]
            Driver::Gone => 0,
            Driver::Poll(_) => 0,
        };
        pool.unreserve(reserved.saturating_sub(made));
        let shared = Arc::new(Shared {
            inbox: Inbox::new(),
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
            io_closed: AtomicBool::new(false),
            counters: Counters::new(driver.backend(), pool.cap(), owner.map(|(_, index)| index)),
            runtime: std::sync::OnceLock::new(),
            writes: AtomicUsize::new(0),
            released: AtomicBool::new(false),
            timer_at: AtomicU64::new(u64::MAX),
            parks: AtomicU64::new(0),
        });
        counters::register(&shared);
        Ok(Self {
            shared,
            queue: RefCell::new(VecDeque::new()),
            lifo: Cell::new(None),
            lifo_streak: Cell::new(0),
            until_inbox: Cell::new(super::tuning::Tuning::default().inbox_interval),
            since_drive: Cell::new(0),
            last_drive: Cell::new(super::quartz::Quartz::now()),
            polls: Cell::new(0),
            sleeps: Cell::new(0),
            slept: Cell::new(Duration::ZERO),
            remote_wakes: Cell::new(0),
            looks: Cell::new(0),
            spin: Spin::new(),
            owner: Cell::new(owner),
            monitor: None,
            home: core::cell::OnceCell::new(),
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
                IoOp::RingAttach(token, lease) => {
                    let source = self.sources.borrow().get(token).cloned();
                    if let (Some(ring), Some(source)) = (self.ring(), source) {
                        source.ring_attached(ring, &self.shared, &lease, token);
                    }
                    drop(lease);
                }
                #[cfg(ring)]
                IoOp::RingDetach(slot, out, lease) => {
                    if let Some(ring) = self.ring() {
                        ring.detach(slot, out, lease);
                    }
                }
                #[cfg(ring)]
                IoOp::RingKick(source) => source.ring_kick(self),
                #[cfg(not(ring))]
                IoOp::Touch(source) => source.touch(self),
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
        if self.tuning.lifo && !info.woken_while_running && super::context::in_task() {
            if let Some(previous) = self.lifo.replace(Some(runnable)) {
                self.queue.borrow_mut().push_back(previous);
            }
        } else {
            self.queue.borrow_mut().push_back(runnable);
        }
    }

    /// The next runnable, in the loop's order.
    fn next(&self) -> Option<Runnable> {
        let left = self.until_inbox.get().wrapping_sub(1);
        if left == 0 {
            self.until_inbox.set(self.tuning.inbox_interval);
            if let Some(r) = self.pop_inbox() {
                return Some(r);
            }
        } else {
            self.until_inbox.set(left);
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

    // Inlined into the run loop's `next` wherever the codegen units put the
    // two: it is looked at on every run that finds the local queue empty.
    #[inline]
    fn pop_inbox(&self) -> Option<Runnable> {
        // SAFETY: the shard holds the inbox's taker's role until it goes
        // (`stop` lets go of it, last).
        let popped = match unsafe { self.shared.inbox.pop() } {
            Pop::Item(runnable) => Some(runnable),
            // A push under way counts as work: the loop comes back. Then the
            // bound tasks of the thread that runs the shard: a runner's, or
            // the ones of this shard, a thread's own.
            Pop::Empty | Pop::Busy => super::context::pop_home().or_else(|| {
                // SAFETY: an own shard runs on its thread only, its home's.
                self.home.get().and_then(|home| unsafe { home.pop() })
            }),
        };
        let runnable = popped?;
        self.remote_wakes.set(self.remote_wakes.get() + 1);
        Some(runnable)
    }

    /// Whether other threads handed the shard work it has not taken yet, as
    /// the taker of its inbox (and of its runner's home) sees it: the look of
    /// its spin and the one a park makes once more.
    /// [`Shared::has_remote_work`], a look from any thread, misses the tasks
    /// this shard's last pop left behind its end as it put an inbox's stub
    /// back ([`Inbox::is_drained`]).
    fn has_remote_work(&self) -> bool {
        let shared = &self.shared;
        // SAFETY: the shard holds the inbox's taker's role until it goes
        // (`stop` lets go of it, last).
        let drained = unsafe { shared.inbox.is_drained() }
            && super::context::home_is_drained()
            // SAFETY: as in `pop_inbox`.
            && self.home.get().is_none_or(|home| unsafe { home.is_drained() });
        !drained || !shared.timer_ops.is_empty() || !shared.io_ops.is_empty()
    }

    /// Runnables queued on this thread now, the LIFO slot's included.
    fn queued(&self) -> usize {
        let lifo = self.lifo.take();
        let n = self.queue.borrow().len() + usize::from(lifo.is_some());
        self.lifo.set(lifo);
        n
    }

    fn has_local_work(&self) -> bool {
        // SAFETY: a shard's cells are its thread's alone, and no reference into the slot is live.
        unsafe { (*self.lifo.as_ptr()).is_some() || !self.queue.borrow().is_empty() }
    }

    /// Applies registration changes and fires due timers, then looks at the
    /// poller (waiting up to `wait`, or until the next timer) and completes
    /// the IO of the sources that became ready.
    fn drive(&self, wait: Option<Duration>) -> io::Result<()> {
        // The run loop's look between tasks, or its park (which may not wait
        // after all, its timer due): a park publishes the counts always, the
        // shard about to go quiet, and a look once every few.
        self.last_drive.set(super::quartz::Quartz::now());
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
                            source.on_event(token, readiness, self);
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
                    unsafe { crate::io::source::port::complete(entry, Some(self)) };
                })?;
                Ok(events)
            }
            #[cfg(ring)]
            Driver::Ring(ring) => ring.wait(Some(self), timeout).map(|(handled, _)| handled),
            #[cfg(ring)]
            Driver::Gone => Ok(0),
        }
    }

    /// The kernel interface this shard runs its IO on.
    pub(crate) fn io_backend(&self) -> crate::IoBackend {
        self.driver.backend()
    }

    /// Whether something outside this thread can bring it work within a spin's time: a job on
    /// the runtime's blocking pool (any shard's: the pool does not say whose), or a finite
    /// operation of the ring still out. Work posted by other threads is not foreseen, so a shard
    /// with neither parks at once.
    fn work_may_come_soon(&self) -> bool {
        if self
            .tuning
            .blocking
            .as_ref()
            .is_some_and(|jobs| jobs.load(Ordering::Relaxed) > 0)
        {
            return true;
        }
        #[cfg(ring)]
        return self
            .ring()
            .is_some_and(ring::ShardRing::expects_completions);
        #[cfg(not(ring))]
        false
    }

    /// The shard's ring, if its IO runs on one.
    #[cfg(ring)]
    pub(crate) fn ring(&self) -> Option<&ring::ShardRing> {
        match &self.driver {
            Driver::Ring(ring) => Some(ring),
            Driver::Poll(_) | Driver::Gone => None,
        }
    }

    /// Completes the IO that is ready now, without waiting: whether there
    /// was any.
    #[expect(
        clippy::panic,
        reason = "a failure of the poller itself cannot be handed to any task: it ends the shard loudly"
    )]
    fn io_ready_now(&self) -> bool {
        match self.poll_io(usize::MAX, Some(Duration::ZERO)) {
            Ok(events) => events > 0,
            Err(err) => panic!("the shard's poller failed: {err}"),
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
