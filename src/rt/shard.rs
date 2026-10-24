//! A shard: one thread's run queue, timers and poller, and the half of it
//! other threads reach.
//!
//! [`Shared`] is what any thread may touch: the inbox other threads push
//! runnables into, the park handshake, the notifier that wakes the poller,
//! and the count of live tasks placement reads. [`Shard`] is the rest, owned
//! by the thread that runs it: the local run queue, the LIFO slot, the timer
//! wheel and the poller. Only that thread touches it, so it is plain cells.
//!
//! # The run loop
//!
//! [`Shard::block_on`] polls one future and, between its polls, runs the
//! shard's tasks: the LIFO slot first (a task woken by the task that just
//! ran, at most [`LIFO_MAX`] times in a row), then the local queue, then the
//! inbox (first every [`INBOX_INTERVAL`] runs, so remote work is not
//! starved). Every [`DRIVER_INTERVAL`] runs, and whenever the queues are
//! empty, it fires due timers and polls the poller without waiting; with
//! nothing to run it parks until the next timer, a remote push or a wake of
//! the future. Workers run the same loop on a future that ends at shutdown.
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
use super::park::{Notifier, Poller};
use super::task::{Runnable, ScheduleInfo};
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicBool, AtomicUsize, Ordering};
use crate::time::wheel::Wheel;

/// Runs of the LIFO slot in a row before its task goes to the back.
pub(crate) const LIFO_MAX: u32 = 3;
/// The inbox is looked at first once every this many runs.
pub(crate) const INBOX_INTERVAL: u32 = 31;
/// Timers and the poller are looked at once every this many runs.
pub(crate) const DRIVER_INTERVAL: u32 = 61;

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
    /// The waker of the worker's loop, woken by `stop`.
    stop_waker: AtomicWaker,
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

    /// Whether the shard stopped.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Stops the shard: a worker's loop ends at its next look.
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.stop_waker.wake();
        self.unpark();
    }

    /// Completes once the shard is stopped: the future a worker's loop runs.
    pub(crate) fn stopped(&self) -> impl Future<Output = ()> + '_ {
        core::future::poll_fn(move |cx| {
            if self.is_stopped() {
                return Poll::Ready(());
            }
            self.stop_waker.register(cx.waker());
            if self.is_stopped() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }

    fn has_remote_work(&self) -> bool {
        !self.inbox.is_empty() || !self.local_inbox.is_empty()
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
            stop_waker: AtomicWaker::new(),
        });
        Ok(Self {
            shared,
            queue: RefCell::new(VecDeque::new()),
            lifo: Cell::new(None),
            lifo_streak: Cell::new(0),
            tick: Cell::new(0),
            since_drive: Cell::new(0),
            in_task: Cell::new(false),
            depth: Cell::new(0),
            limits,
            poller: RefCell::new(poller),
            wheel: RefCell::new(Wheel::new()),
            epoch: Instant::now(),
            fired: RefCell::new(Vec::new()),
        })
    }

    /// Whether `shared` is this shard's.
    pub(crate) fn is(&self, shared: &Arc<Shared>) -> bool {
        Arc::ptr_eq(&self.shared, shared)
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

    /// The wheel's time for `at`, rounded up: a deadline computed from it is
    /// never early.
    pub(crate) fn wheel_time(&self, at: Instant) -> u64 {
        let since = at.saturating_duration_since(self.epoch);
        u64::try_from(since.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX)
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

    /// Fires due timers, then looks at the poller (waiting up to `wait`, or
    /// until the next timer).
    fn drive(&self, wait: Option<Duration>) -> io::Result<()> {
        self.fire_timers();
        let timeout = match (wait, self.next_timer()) {
            (Some(w), Some(t)) => Some(w.min(t)),
            (w, t) => w.or(t),
        };
        self.poller.borrow_mut().wait(timeout)?;
        self.fire_timers();
        Ok(())
    }

    /// How long until the next timer, if any.
    fn next_timer(&self) -> Option<Duration> {
        let next = self.wheel.borrow().next_deadline()?;
        let now = self.wheel_time(Instant::now());
        Some(Duration::from_millis(next.saturating_sub(now)))
    }

    fn fire_timers(&self) {
        if self.wheel.borrow().armed() == 0 {
            return;
        }
        let now = self.wheel_time(Instant::now());
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
            // Nothing to run: due timers may have made work.
            self.fire_timers();
            if self.has_local_work() || signal.woken.load(Ordering::SeqCst) {
                continue;
            }
            self.since_drive.set(0);
            let shared = &self.shared;
            if shared
                .park
                .begin_park(|| shared.has_remote_work() || signal.woken.load(Ordering::SeqCst))
            {
                self.drive_or_report(None);
                shared.park.end_park();
            }
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
        self.shared.stopped.store(true, Ordering::SeqCst);
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
        let shard = Shard::new(Limits { max_nesting: 8 }).unwrap();
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
        let shard = Shard::new(Limits { max_nesting: 8 }).unwrap();
        let _current = context::enter(&shard);
        let start = Instant::now();
        // Nothing but the timer can wake the loop: if it parked without a
        // deadline, this would never return.
        shard.block_on(sleep_on(&shard, start + Duration::from_millis(20)));
        assert!(start.elapsed() >= Duration::from_millis(20));
    }
}
