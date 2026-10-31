//! The run loop of a shard (see the module docs of `shard`): `block_on`, a
//! worker's loop, a turn driven by hand, and what a shard taken from its
//! runner does with its own queues.
//!
//! Each poll the loop makes, of a task or of its future, runs through
//! [`context::poll`]: outside the shard, then back in. A poll may end with
//! the shard taken from the thread (`rt::handoff`); the frame of the loop
//! that made it then touches the shard no more. A worker's loop ends; a
//! `block_on` moves its future to the thread's own shard, its waker aimed
//! there first, and finishes it there.

use core::future::Future;
use core::pin::{Pin, pin};
use core::task::{Context, Poll, Waker};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Shard;
use super::signal::Signal;
use crate::rt::context::{self, Polled};
use crate::rt::quartz::Quartz;
use crate::rt::task::Runnable;
use crate::rt::tuning::QUARTZ_EVERY;
use crate::sync::internal::Ordering;

/// The shard was taken from this thread while the loop ran.
pub(crate) struct Detached;

/// How a worker's loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Worked {
    /// The shard stopped, or drained of its tasks.
    Stopped,
    /// The shard was taken from this thread.
    Detached,
}

impl Shard {
    /// Polls a runnable's task.
    #[inline(always)]
    pub(super) fn run(&self, runnable: Runnable) -> Polled {
        context::poll(&self.shared, || {
            runnable.run();
        })
        .1
    }

    /// Runs `future` to its end on this shard, running the shard's tasks,
    /// timers and poller between its polls; on the thread's own shard if this
    /// one is taken from the thread meanwhile.
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        // Pinned before the nesting's closure, which then holds a reference.
        let mut future = pin!(future);
        context::deeper(self.limits.max_nesting, || {
            let signal = self.signal();
            match self.run_loop(future.as_mut(), &signal) {
                Ok(output) => {
                    self.keep_signal(signal);
                    output
                }
                Err(Detached) => finish_on_own(future, &signal),
            }
        })
    }

    /// A worker's loop: until the shard stops, drains of its tasks, or is
    /// taken from this thread.
    pub(crate) fn run_worker(&self) -> Worked {
        // The IO a task queued on this shard (a receive in the ring's
        // submission queue) reaches the kernel before any task runs: a shard
        // handed on again and again, each runner's first task blocking, would
        // otherwise be handed on before any runner reached the driver, and
        // the operation would wait out `driver_interval` runs.
        self.count_again(self.since_drive.get());
        self.drive_or_report(Some(Duration::ZERO));
        let stopped = pin!(self.shared.stopped());
        let signal = self.signal();
        match self.run_loop(stopped, &signal) {
            Ok(()) => Worked::Stopped,
            Err(Detached) => Worked::Detached,
        }
    }

    /// The waker of a `block_on` future on this shard: the one the last call
    /// here left, if nothing else holds it any more, else a new one.
    pub(super) fn signal(&self) -> Arc<Signal> {
        if let Some(mut signal) = self.signal.take()
            && Arc::get_mut(&mut signal).is_some_and(Signal::reset)
        {
            return signal;
        }
        Arc::new(Signal::new(&self.shared))
    }

    /// Keeps `signal`, whose future is done, for the next call's.
    pub(super) fn keep_signal(&self, signal: Arc<Signal>) {
        self.signal.set(Some(signal));
    }

    /// [`block_on`](Self::block_on) for a future already pinned and woken
    /// through `signal`, on this shard, a thread's own, entered for the
    /// outermost `block_on` of the thread; then leaves (see `going`).
    pub(crate) fn run_then_leave<F: Future>(
        &self,
        mut future: Pin<&mut F>,
        signal: &Arc<Signal>,
    ) -> F::Output {
        struct Leaving<'a>(&'a Shard);
        impl Drop for Leaving<'_> {
            fn drop(&mut self) {
                self.0.leave();
            }
        }
        let _leaving = Leaving(self);
        loop {
            // A thread's own shard is never taken: nobody watches it.
            if let Ok(output) = self.run_loop(future.as_mut(), signal) {
                return output;
            }
        }
    }

    /// The loop: polls `future` when it is woken and, between its polls, runs
    /// the shard's tasks, timers and poller, until `future` is ready or the
    /// shard is taken from this thread.
    fn run_loop<F: Future>(
        &self,
        mut future: Pin<&mut F>,
        signal: &Arc<Signal>,
    ) -> Result<F::Output, Detached> {
        let waker = Waker::from(Arc::clone(signal));
        let mut cx = Context::from_waker(&waker);
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
                let (poll, polled) = context::poll(&self.shared, || {
                    let budget = crate::coop::enter();
                    let poll = future.as_mut().poll(&mut cx);
                    crate::coop::leave(budget);
                    poll
                });
                if let Poll::Ready(output) = poll {
                    return Ok(output);
                }
                if polled == Polled::Detached {
                    return Err(Detached);
                }
                if signal.woken.load(Ordering::Relaxed) {
                    owed = self.queued();
                }
            }
            if let Some(runnable) = self.next() {
                owed = owed.saturating_sub(1);
                if self.run(runnable) == Polled::Detached {
                    return Err(Detached);
                }
                let since = self.since_drive.get() + 1;
                // Past the run count, or (checked once every `QUARTZ_EVERY`
                // runs, by a mask) past the time bound since the last look:
                // a few long runs hold the driver off as much as many short.
                if since >= self.tuning.driver_interval
                    || (since & (QUARTZ_EVERY - 1) == 0 && self.drive_overdue())
                {
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
            if self.spin.run(self.work_may_come_soon(), || {
                self.has_remote_work()
                    || signal.woken.load(Ordering::Relaxed)
                    || self.io_ready_now()
            }) {
                continue;
            }
            self.before_park();
            // A park a blocking job's end cuts short says nothing of other work coming soon.
            let pool_out = self.blocking_out();
            if shared
                .park
                .begin_park(|| self.has_remote_work() || signal.woken.load(Ordering::SeqCst))
            {
                let slept = Instant::now();
                if let Some(park) = &self.tuning.hooks.park {
                    park();
                }
                self.drive_or_report(None);
                shared.park.end_park();
                self.parked_ended();
                if let Some(unpark) = &self.tuning.hooks.unpark {
                    unpark();
                }
                self.slept_for(slept.elapsed(), !pool_out);
            }
            self.after_park();
        }
    }

    /// One turn of the shard, for a caller driving it by hand: runs every
    /// task ready now, waits once on the poller (up to `wait`, at once if
    /// work is ready; at most `max` events) and runs what the IO woke.
    /// Returns how many IO events it handled; none once the shard was taken
    /// from this thread during the turn.
    pub(crate) fn turn(&self, max: usize, wait: Option<Duration>) -> std::io::Result<usize> {
        if self.run_ready() == Polled::Detached {
            return Ok(0);
        }
        if !self.shared.io_ops.is_empty() {
            self.apply_io_ops();
        }
        self.fire_timers();
        let shared = &self.shared;
        let quiet = wait != Some(Duration::ZERO) && !self.has_local_work();
        if quiet {
            self.before_park();
        }
        let sleeps = quiet && shared.park.begin_park(|| self.has_remote_work());
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
            self.parked_ended();
        }
        if quiet {
            self.after_park();
        }
        let events = result?;
        self.fire_timers();
        if self.run_ready() == Polled::Detached {
            return Ok(0);
        }
        Ok(events)
    }

    /// Runs every task that is ready now, and the ones they wake, until the
    /// shard is taken from this thread.
    pub(super) fn run_ready(&self) -> Polled {
        while let Some(runnable) = self.next() {
            let polled = self.run(runnable);
            if polled == Polled::Detached {
                return polled;
            }
            self.polls.set(self.polls.get() + 1);
        }
        Polled::In
    }

    /// Whether more than the drive bound has passed since the last look at
    /// timers and the poller, by the coarse clock.
    fn drive_overdue(&self) -> bool {
        Quartz::now().wrapping_sub(self.last_drive.get()) >= self.tuning.drive_bound
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
    #[expect(
        clippy::panic,
        reason = "a failure of the poller itself cannot be handed to any task: it ends the shard loudly"
    )]
    fn drive_or_report(&self, wait: Option<Duration>) {
        if let Err(err) = self.drive(wait) {
            panic!("the shard's poller failed: {err}");
        }
    }

    /// The shard was taken from its runner, and is the caller's (the
    /// monitor's): the runnables in its own queues go back to their tasks'
    /// schedules (to the shard's inbox, or a bound task's home), and the run
    /// of the LIFO slot starts over.
    pub(crate) fn hand_over_queues(&self) {
        let lifo = self.lifo.take();
        let queued: Vec<Runnable> = self.queue.borrow_mut().drain(..).collect();
        self.lifo_streak.set(0);
        for runnable in lifo.into_iter().chain(queued) {
            runnable.schedule();
        }
    }
}

/// A `block_on` future whose shard was taken from this thread: it finishes
/// on the thread's own shard, its waker aimed there first, then looked at
/// once more before the loop sleeps (the loop starts with that look).
#[expect(
    clippy::panic,
    reason = "documented under `# Panics` of `block_on`: the thread's shard cannot start"
)]
fn finish_on_own<F: Future>(future: Pin<&mut F>, signal: &Arc<Signal>) -> F::Output {
    let own = match context::own_shard() {
        Ok(own) => own,
        Err(err) => panic!("this thread's shard could not start: {err}"),
    };
    signal.target.move_to(&own.shared);
    let _current = context::enter(&own);
    let inner = context::runtime();
    let _guest = inner
        .as_ref()
        .map(|inner| crate::rt::paused::Guest::enter(inner, &own.shared));
    #[cfg(port)]
    own.take_port_back();
    own.run_then_leave(future, signal)
}
