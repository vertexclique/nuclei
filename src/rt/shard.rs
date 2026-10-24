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
//! starved). Every [`DRIVER_INTERVAL`] runs, and whenever the queues are
//! empty, it fires due timers and polls the poller without waiting, which
//! completes the IO of every source that became ready (see `io::source`). With
//! nothing to run it spins briefly (see [`Spin`]), then parks until the next
//! timer, a remote push or a wake of the future. Workers run the same loop
//! on a future that ends at shutdown.
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
use std::task::Wake;
use std::time::{Duration, Instant};

use kovan_queue::seg_queue::SegQueue;

use super::handshake::ParkState;
use super::park::{Notifier, Poller, Registrar};
use super::sources::Sources;
use super::task::{Runnable, ScheduleInfo};
use crate::buf::{self, Pool};
use crate::io::Source;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicBool, AtomicUsize, Ordering};
use crate::time::TimerOp;
use crate::time::wheel::Wheel;

/// Runs of the LIFO slot in a row before its task goes to the back.
pub(crate) const LIFO_MAX: u32 = 3;
/// The inbox is looked at first once every this many runs.
pub(crate) const INBOX_INTERVAL: u32 = 31;
/// Timers and the poller are looked at once every this many runs.
pub(crate) const DRIVER_INTERVAL: u32 = 61;

/// The most pause hints a shard spins before it parks.
const SPIN_MAX: u32 = 2048;
/// The spin a fresh shard starts with.
const SPIN_START: u32 = 256;
/// The spin a shard whose park was cut short starts again with.
const SPIN_RESTART: u32 = 64;
/// A park that ended this soon would have been caught by a spin.
const SHORT_PARK: Duration = Duration::from_micros(50);
/// The longest gap between two looks of a spin, in pauses: each look reads
/// lines the waker writes, so looking seldom leaves it to run at full speed.
const SPIN_GAP_MAX: u32 = 64;

/// How long a shard spins before it parks, adapted to what spins see.
///
/// Work that arrives within a spin costs neither a park nor the waker's
/// notification, and IO that becomes ready within it (each look asks the
/// poller too, without waiting) costs no kernel wake of the thread. A spin
/// that found work doubles the budget; one that ran out halves it, so a
/// shard whose work comes only after long gaps stops spinning; a park that
/// ended soon after it began sets it going again. The budget counts pause
/// hints, not time, and is a hint: it only sizes the next spin.
pub(crate) struct Spin {
    budget: Cell<u32>,
}

impl Spin {
    fn new() -> Self {
        Self {
            budget: Cell::new(SPIN_START),
        }
    }

    /// Spins until `found` says work is there (true) or the budget is spent
    /// (false).
    fn run(&self, mut found: impl FnMut() -> bool) -> bool {
        let budget = self.budget.get();
        let mut spent = 0;
        let mut gap = 1;
        while spent < budget {
            for _ in 0..gap {
                core::hint::spin_loop();
            }
            spent += gap;
            gap = (gap * 2).min(SPIN_GAP_MAX);
            if found() {
                self.budget.set((budget * 2).clamp(SPIN_RESTART, SPIN_MAX));
                return true;
            }
        }
        self.budget.set(budget / 2);
        false
    }

    /// The shard slept for `slept`: a short sleep means work came right
    /// after the spin gave up.
    fn parked_for(&self, slept: Duration) {
        if slept < SHORT_PARK {
            self.budget.set(self.budget.get().max(SPIN_RESTART));
        }
    }
}

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
    registrar: Registrar,
    /// Registrations other threads ask of the shard, applied before it
    /// polls.
    io_ops: SegQueue<IoOp>,
}

/// A registration change another thread asks of a shard.
pub(crate) enum IoOp {
    /// Watch a source for a thread that runs no shard.
    Register(Arc<Source>),
    /// A source went (its kernel registration is already gone): free its
    /// slot.
    Release(u64),
}

impl Shared {
    /// Hands a runnable to the shard from another thread and wakes it if it
    /// sleeps. `bound`: the task must run on the shard's thread only.
    pub(crate) fn push_remote(&self, runnable: Runnable, bound: bool) {
        if bound {
            self.local_inbox.push(runnable);
        } else {
            self.inbox.push(runnable);
        }
        // The queue's push is sequentially consistent: the shard either finds
        // the runnable after it parks, or is parked now.
        self.park.wake(|| self.notifier.notify());
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

    /// Adds and removes the kernel registrations of the shard's poller.
    pub(crate) fn registrar(&self) -> &Registrar {
        &self.registrar
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
    spin: Spin,
    /// The runtime and the place this shard is a worker of, set when its
    /// worker thread starts: what `spawn` uses to keep a task local.
    owner: Cell<Option<(u64, usize)>>,
    /// A task is being polled: a wake it makes goes to the LIFO slot.
    in_task: Cell<bool>,
    depth: Cell<usize>,
    limits: Limits,
    poller: RefCell<Poller>,
    wheel: RefCell<Wheel>,
    /// The wheel's zero.
    epoch: Instant,
    /// Wakers of fired timers, woken once the wheel is let go; reused.
    fired: RefCell<Vec<Waker>>,
    /// The IO sources the poller watches, by token.
    sources: RefCell<Sources>,
    /// Where the shard's reads land and its writes wait.
    pool: Pool,
}

impl Shard {
    pub(crate) fn new(limits: Limits) -> io::Result<Self> {
        let poller = Poller::new()?;
        let shared = Arc::new(Shared {
            inbox: SegQueue::new(),
            local_inbox: SegQueue::new(),
            park: ParkState::new(),
            notifier: poller.notifier(),
            load: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            stop_waker: AtomicWaker::new(),
            timer_ops: SegQueue::new(),
            registrar: poller.registrar(),
            io_ops: SegQueue::new(),
        });
        Ok(Self {
            shared,
            queue: RefCell::new(VecDeque::new()),
            lifo: Cell::new(None),
            lifo_streak: Cell::new(0),
            tick: Cell::new(0),
            since_drive: Cell::new(0),
            spin: Spin::new(),
            owner: Cell::new(None),
            in_task: Cell::new(false),
            depth: Cell::new(0),
            limits,
            poller: RefCell::new(poller),
            wheel: RefCell::new(Wheel::new()),
            epoch: Instant::now(),
            fired: RefCell::new(Vec::new()),
            sources: RefCell::new(Sources::default()),
            pool: Pool::new(limits.buffer_cap),
        })
    }

    /// Watches `source` from now on, under a new token. The caller moved
    /// the source's registration to registering.
    pub(crate) fn register_source(&self, source: &Arc<Source>) -> io::Result<()> {
        let token = match self.sources.borrow_mut().insert(Arc::clone(source)) {
            Ok(token) => token,
            Err(err) => {
                source.registration_abandoned();
                return Err(err);
            }
        };
        let attached = source.attach(&self.shared, token);
        if attached.is_err() {
            let gone = self.sources.borrow_mut().remove(token);
            drop(gone);
        }
        attached
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
            }
        }
    }

    /// Whether `shared` is this shard's.
    pub(crate) fn is(&self, shared: &Arc<Shared>) -> bool {
        Arc::ptr_eq(&self.shared, shared)
    }

    /// Marks this shard as worker `index` of runtime `runtime`.
    pub(crate) fn set_owner(&self, runtime: u64, index: usize) {
        self.owner.set(Some((runtime, index)));
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

    /// The shard's timers.
    pub(crate) fn wheel(&self) -> &RefCell<Wheel> {
        &self.wheel
    }

    /// The wheel's time for the deadline `at`, rounded up to its
    /// millisecond: a timer never fires before its deadline.
    pub(crate) fn wheel_time(&self, at: Instant) -> u64 {
        let since = at.saturating_duration_since(self.epoch);
        u64::try_from(since.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX)
    }

    /// The wheel's time now, rounded down: the wheel never runs ahead of the
    /// clock.
    fn wheel_now(&self) -> u64 {
        let since = Instant::now().saturating_duration_since(self.epoch);
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    }

    /// The next runnable, in the loop's order.
    fn next(&self) -> Option<Runnable> {
        let tick = self.tick.get().wrapping_add(1);
        self.tick.set(tick);
        if tick.is_multiple_of(INBOX_INTERVAL)
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
        self.shared
            .inbox
            .try_pop()
            .or_else(|_| self.shared.local_inbox.try_pop())
            .ok()
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
        if !self.shared.io_ops.is_empty() {
            self.apply_io_ops();
        }
        self.fire_timers();
        let timeout = match (wait, self.next_timer()) {
            (Some(w), Some(t)) => Some(w.min(t)),
            (w, t) => w.or(t),
        };
        self.poll_io(usize::MAX, timeout)?;
        self.fire_timers();
        Ok(())
    }

    /// Waits on the poller up to `timeout` (`None` for no bound) and
    /// completes the IO of the sources that became ready, taking at most
    /// `max` events from the kernel. Returns how many it handled.
    fn poll_io(&self, max: usize, timeout: Option<Duration>) -> io::Result<usize> {
        let sources = self.sources.borrow();
        let mut events = 0;
        self.poller
            .borrow_mut()
            .wait_at_most(max, timeout, |token, readiness| {
                events += 1;
                if let Some(source) = sources.get(token) {
                    source.on_event(token, readiness);
                }
            })?;
        Ok(events)
    }

    /// Completes the IO that is ready now, without waiting: whether there
    /// was any.
    fn io_ready_now(&self) -> bool {
        match self.poll_io(usize::MAX, Some(Duration::ZERO)) {
            Ok(events) => events > 0,
            Err(err) => panic!("the shard's poller failed: {err}"),
        }
    }

    /// How long until the next timer, if any.
    fn next_timer(&self) -> Option<Duration> {
        let next = self.wheel.borrow().next_deadline()?;
        // From the clock's own nanoseconds, so the wait ends at the
        // deadline's millisecond and not one early.
        let at = self.epoch + Duration::from_millis(next);
        Some(at.saturating_duration_since(Instant::now()))
    }

    fn fire_timers(&self) {
        if !self.shared.timer_ops.is_empty() {
            let mut wheel = self.wheel.borrow_mut();
            while let Ok(op) = self.shared.timer_ops.try_pop() {
                op.apply(self, &mut wheel);
            }
        }
        if self.wheel.borrow().armed() == 0 {
            return;
        }
        let now = self.wheel_now();
        // The buffer is taken out, so the wheel and the buffer are let go
        // while the wakers run (a wake may arm a timer again).
        let mut fired = self.fired.take();
        self.wheel.borrow_mut().advance(now, &mut fired);
        for waker in fired.drain(..) {
            waker.wake();
        }
        self.fired.replace(fired);
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
        loop {
            if signal.woken.load(Ordering::Relaxed) && signal.woken.swap(false, Ordering::SeqCst) {
                let outer = self.in_task.replace(true);
                let poll = future.as_mut().poll(&mut cx);
                self.in_task.set(outer);
                if let Poll::Ready(output) = poll {
                    return output;
                }
            }
            if let Some(runnable) = self.next() {
                self.run(runnable);
                let since = self.since_drive.get() + 1;
                if since >= DRIVER_INTERVAL {
                    // Timers and the poller get a look, without a wait.
                    self.since_drive.set(0);
                    self.drive_or_report(Some(Duration::ZERO));
                } else {
                    self.since_drive.set(since);
                }
                continue;
            }
            // Nothing to run: due timers may have made work, and work other
            // threads posted (which the spin and the park count as work) is
            // applied here, so it never keeps the loop from sleeping.
            if !self.shared.io_ops.is_empty() {
                self.apply_io_ops();
            }
            self.fire_timers();
            if self.has_local_work() || signal.woken.load(Ordering::SeqCst) {
                continue;
            }
            self.since_drive.set(0);
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
            if shared
                .park
                .begin_park(|| shared.has_remote_work() || signal.woken.load(Ordering::SeqCst))
            {
                let slept = Instant::now();
                self.drive_or_report(None);
                shared.park.end_park();
                self.spin.parked_for(slept.elapsed());
            }
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
        let sleeps = wait != Some(Duration::ZERO)
            && !self.has_local_work()
            && shared.park.begin_park(|| shared.has_remote_work());
        let wait = if sleeps || wait == Some(Duration::ZERO) {
            wait
        } else {
            Some(Duration::ZERO)
        };
        let timeout = match (wait, self.next_timer()) {
            (Some(w), Some(t)) => Some(w.min(t)),
            (w, t) => w.or(t),
        };
        let result = self.poll_io(max, timeout);
        if sleeps {
            shared.park.end_park();
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
        }
    }

    /// Drives the poller; a failure of the poller itself cannot be handed to
    /// any task, so it ends the shard loudly.
    fn drive_or_report(&self, wait: Option<Duration>) {
        if let Err(err) = self.drive(wait) {
            panic!("the shard's poller failed: {err}");
        }
    }
}

impl Drop for Shard {
    fn drop(&mut self) {
        // Every task still queued here is dropped on this thread, the one its
        // future may be dropped on.
        self.shared.draining.store(true, Ordering::SeqCst);
        self.shared.stopped.store(true, Ordering::SeqCst);
        // The sources registered here lose their driver: each is told, and
        // registers where it is polled next.
        for source in self.sources.get_mut().drain() {
            source.evict();
        }
        drop(self.lifo.take());
        let queue = core::mem::take(&mut *self.queue.borrow_mut());
        drop(queue);
        while let Some(runnable) = self.pop_inbox() {
            drop(runnable);
        }
    }
}

/// The waker of a `block_on` future: marks it woken and wakes the shard.
struct Signal {
    woken: AtomicBool,
    shared: Arc<Shared>,
}

impl Wake for Signal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        self.shared.unpark();
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Limits, Shard};
    use crate::rt::context;

    /// A timer armed in the shard's wheel, done once it fired.
    fn sleep_on(shard: &Shard, at: Instant) -> impl core::future::Future<Output = ()> + '_ {
        let mut key = None;
        core::future::poll_fn(move |cx| {
            let mut wheel = shard.wheel().borrow_mut();
            match key {
                None => {
                    key = Some(wheel.insert(shard.wheel_time(at), cx.waker()));
                }
                Some(k) if wheel.is_fired(k) => {
                    wheel.remove(k);
                    return core::task::Poll::Ready(());
                }
                Some(k) => wheel.set_waker(k, cx.waker()),
            }
            let fired = key.is_some_and(|k| wheel.is_fired(k));
            if fired {
                core::task::Poll::Ready(())
            } else {
                core::task::Poll::Pending
            }
        })
    }

    #[test]
    fn a_timer_fires_from_the_run_loop_and_never_early() {
        let shard = Shard::new(Limits {
            max_nesting: 8,
            buffer_cap: crate::buf::DEFAULT_CAP,
        })
        .unwrap();
        let _current = context::enter(&shard);
        for ms in [1u64, 5, 30] {
            let start = Instant::now();
            let at = start + Duration::from_millis(ms);
            shard.block_on(sleep_on(&shard, at));
            assert!(Instant::now() >= at, "a {ms} ms timer fired early");
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }

    #[test]
    fn the_loop_parks_until_the_next_timer_rather_than_forever() {
        let shard = Shard::new(Limits {
            max_nesting: 8,
            buffer_cap: crate::buf::DEFAULT_CAP,
        })
        .unwrap();
        let _current = context::enter(&shard);
        let start = Instant::now();
        // Nothing but the timer can wake the loop: if it parked without a
        // deadline, this would never return.
        shard.block_on(sleep_on(&shard, start + Duration::from_millis(20)));
        assert!(start.elapsed() >= Duration::from_millis(20));
    }
}

#[cfg(all(test, not(loom)))]
mod spin_tests {
    use std::time::Duration;

    use super::{SPIN_MAX, SPIN_RESTART, SPIN_START, Spin};

    #[test]
    fn a_spin_that_finds_work_doubles_the_budget_and_one_that_runs_out_halves_it() {
        let spin = Spin::new();
        assert_eq!(spin.budget.get(), SPIN_START);
        assert!(spin.run(|| true));
        assert_eq!(spin.budget.get(), SPIN_START * 2);
        assert!(!spin.run(|| false));
        assert_eq!(spin.budget.get(), SPIN_START);
        for _ in 0..20 {
            spin.run(|| true);
        }
        assert_eq!(spin.budget.get(), SPIN_MAX, "capped");
        for _ in 0..20 {
            spin.run(|| false);
        }
        assert_eq!(spin.budget.get(), 0, "an idle shard stops spinning");
        assert!(!spin.run(|| true), "a spent budget does not spin");
    }

    #[test]
    fn a_short_park_starts_the_spin_again() {
        let spin = Spin::new();
        for _ in 0..20 {
            spin.run(|| false);
        }
        spin.parked_for(Duration::from_millis(5));
        assert_eq!(
            spin.budget.get(),
            0,
            "a long sleep: spinning would not have helped"
        );
        spin.parked_for(Duration::from_micros(3));
        assert_eq!(spin.budget.get(), SPIN_RESTART);
    }
}
