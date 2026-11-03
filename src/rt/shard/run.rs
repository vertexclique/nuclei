//! The run loop of a shard (see the module docs of `shard`): `block_on`, a
//! worker's loop, a turn driven by hand, and what a shard taken from its
//! runner does with its own queues.
//!
//! Each poll the loop makes, of a task or of its future, runs through
//! [`context::poll`]: outside the shard, then back in. A poll may end with
//! the shard taken from the thread (`rt::handoff`); the frame of the loop
//! that made it then touches the shard no more, even where that poll
//! finished its future (proofs/tla/handoff, `N_safe`). A worker's loop ends;
//! a `block_on` returns that output, or moves its future to the thread's own
//! shard, its waker aimed there first, and finishes it there.

use core::future::Future;
use core::pin::{Pin, pin};
use core::task::{Context, Poll};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::Shard;
use super::signal::Signal;
use super::spin::Spun;
use crate::rt::context::{self, Polled};
use crate::rt::quartz::Quartz;
use crate::rt::task::Runnable;
use crate::rt::tuning::QUARTZ_EVERY;
use crate::sync::internal::Ordering;

#[cfg(test)]
thread_local! {
    /// Run in the run loop's idle pass on this thread just before its park: what it makes ready
    /// then backs the park out.
    pub(crate) static BEFORE_PARK: core::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { core::cell::RefCell::new(None) };
}

/// Runs [`BEFORE_PARK`], taken out of its cell while it runs.
#[cfg(test)]
fn before_park_hook() {
    let Some(mut hook) = BEFORE_PARK.with(|hook| hook.borrow_mut().take()) else {
        return;
    };
    hook();
    BEFORE_PARK.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(hook);
        }
    });
}

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
        context::run_task(&self.shared, || {
            runnable.run();
        })
    }

    /// Runs `future` to its end on this shard, running the shard's tasks,
    /// timers and poller between its polls; on the thread's own shard if this
    /// one is taken from the thread meanwhile.
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        crate::sync::internal::freeze_frames();
        // Pinned before the nesting's closure, which then holds a reference.
        let mut future = pin!(future);
        let unwinding = super::timers::ClearOnUnwind;
        let output = context::deeper(self.limits.max_nesting, || {
            let signal = self.signal();
            match self.run_loop(future.as_mut(), &signal) {
                Ok(output) => {
                    // The poll that finished the future may have found the shard taken: it is
                    // another thread's then, and its cell keeps no waker of this call.
                    if !context::detached_from(&self.shared) {
                        self.keep_signal(signal);
                    }
                    output
                }
                Err(Detached) => finish_on_own(future, &signal),
            }
        });
        core::mem::forget(unwinding);
        output
    }

    /// A worker's loop: until the shard stops, drains of its tasks, or is
    /// taken from this thread.
    pub(crate) fn run_worker(&self) -> Worked {
        // The IO a task queued on this shard (a receive in the ring's
        // submission queue) reaches the kernel before any task runs: a shard
        // handed on again and again, each runner's first task blocking, would
        // otherwise be handed on before any runner reached the driver, and
        // the operation would wait out `driver_interval` runs.
        self.count_again(self.since_drive());
        self.drive_or_report(Some(Duration::ZERO));
        let stopped = pin!(self.shared.stopped());
        let signal = self.signal();
        let mut stopped = stopped;
        // A timer's waker that panics as the loop fires it must not end the
        // worker and lose its shard: the loop runs again. Any other panic goes on.
        let ended = loop {
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.run_loop(stopped.as_mut(), &signal)
            }));
            match ran {
                Ok(ended) => break ended,
                Err(panic) if super::timers::take_fire_panic() => crate::rt::caught::lost(
                    format_args!("a timer's waker panicked as it fired"),
                    panic,
                ),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        };
        match ended {
            // Stopped in the very poll that found the take, the thread lost the shard all the
            // same: its bound tasks and its life are seen to as a detached thread's.
            Ok(()) if !context::detached_from(&self.shared) => Worked::Stopped,
            Ok(()) | Err(Detached) => Worked::Detached,
        }
    }

    /// The waker of a `block_on` future on this shard: the one the last call
    /// here left, if nothing else holds it any more, else a new one.
    pub(super) fn signal(&self) -> Arc<Signal> {
        // No `Weak` of a signal is made, so a count of one is the only
        // holder: a waker another thread dropped last did so before the
        // acquire, and none is left to wake the next future.
        if let Some(signal) = self.signal.take()
            && Arc::strong_count(&signal) == 1
        {
            crate::sync::internal::fence(Ordering::Acquire);
            if signal.reset() {
                return signal;
            }
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
        let unwinding = super::timers::ClearOnUnwind;
        loop {
            // A thread's own shard is never taken: nobody watches it.
            if let Ok(output) = self.run_loop(future.as_mut(), signal) {
                core::mem::forget(unwinding);
                return output;
            }
        }
    }

    /// The loop: polls `future` when it is woken and, between its polls, runs
    /// the shard's tasks, timers and poller, until `future` is ready or the
    /// shard is taken from this thread. A poll may do both: it answers the
    /// output, and a caller whose shard may be watched (`block_on`,
    /// `run_worker`) looks at the take before it touches the shard
    /// ([`context::detached_from`]); `run_then_leave`'s, a thread's own, is
    /// never taken.
    fn run_loop<F: Future>(
        &self,
        mut future: Pin<&mut F>,
        signal: &Arc<Signal>,
    ) -> Result<F::Output, Detached> {
        let waker = Signal::waker(signal);
        let mut cx = Context::from_waker(&waker);
        // Turns in which the future woke itself and no task ran, and parks that backed out with
        // the spin cold.
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
                // One decrement a run; the runs that weigh a look are the
                // count's stops.
                let left = self.drive_left.get() - 1;
                if left == 0 {
                    self.drive_count_stop();
                } else {
                    self.drive_left.set(left);
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
                self.count_unlooked(&mut self_woken);
                continue;
            }
            let shared = &self.shared;
            // The spin looks at the poller too: IO that comes within it
            // costs neither a park nor the kernel's wake of this thread. The
            // queues of posted work, which a moment ago held nothing (they
            // were applied above), are looked at last, past ready IO; the
            // inbox and the homes, which the run queue's last pop just found
            // empty, and the future's wake, looked at just above, from the
            // second look on. So the first look asks the poller before
            // anything else, and a spin that finds work has looked: its hit
            // starts the count of runs over (proofs/tla/loop/RunLoop.tla,
            // `L_spin_woken`).
            let mut first = true;
            let spun = self.spin.run(
                || self.work_may_come_soon(),
                || {
                    (!core::mem::take(&mut first)
                        && (self.has_remote_tasks() || signal.woken.load(Ordering::Relaxed)))
                        || self.io_ready_now()
                        || self.has_posted_ops()
                },
            );
            if spun == Spun::Hit {
                self.count_again(self.since_drive());
                continue;
            }
            #[cfg(test)]
            before_park_hook();
            self.before_park();
            // A park a blocking job's end cuts short says nothing of other work coming soon.
            let pool_out = self.blocking_out();
            if self.begin_park(|| self.has_remote_work() || signal.woken.load(Ordering::SeqCst)) {
                self.count_again(self.since_drive());
                let slept = Instant::now();
                self.park_began(slept);
                self.tuning.hooks.parked();
                self.drive_or_report(None);
                shared.park.end_park();
                self.parked_ended();
                self.tuning.hooks.unparked();
                self.slept_for(slept.elapsed(), !pool_out);
                self.park_ended();
            } else if spun == Spun::Missed {
                // Every look of the spin asked the poller.
                self.count_again(self.since_drive());
            } else {
                self.count_unlooked(&mut self_woken);
            }
            self.after_park();
        }
    }

    /// Says the shard parks and looks for work once more (`Handshake::begin_park`): whether it
    /// parks. A shard that backs out runs on, so its runtime's monitor, which may have taken the
    /// moment it said it parked for every worker parked and gone to sleep, is rung as a park's end
    /// rings it: else the monitor sleeps while the shard runs, and rescues no poll that blocks
    /// (proofs/tla/handoff/Asleep.tla).
    #[inline(always)]
    pub(crate) fn begin_park(&self, has_work: impl FnOnce() -> bool) -> bool {
        let parks = self.shared.park.begin_park(has_work);
        if !parks {
            self.parked_ended();
        }
        parks
    }

    /// One turn of the shard, for a caller driving it by hand: runs the
    /// tasks ready now, waits once on the poller (up to `wait`, at once if
    /// work is ready; at most `max` events) and runs what the IO woke, each
    /// run of tasks bounded by [`run_ready`](Self::run_ready). Returns how
    /// many IO events it handled; none once the shard was taken from this
    /// thread during the turn.
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
        let sleeps = quiet && self.begin_park(|| self.has_remote_work());
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
        let slept = sleeps.then(Instant::now);
        if let Some(slept) = slept {
            self.park_began(slept);
        }
        let result = self.poll_io(max, timeout);
        if let Some(slept) = slept {
            shared.park.end_park();
            self.parked_ended();
            self.slept_for(slept.elapsed(), false);
            self.park_ended();
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

    /// Runs the tasks that are ready now, and the ones they wake, at most
    /// the driver interval of them, or until the shard is taken from this
    /// thread: tasks that keep waking each other leave the turn to its look
    /// at the timers and the poller, and to its return
    /// (proofs/tla/loop/RunLoop.tla, `Mode = "turn"`).
    pub(super) fn run_ready(&self) -> Polled {
        for _ in 0..self.tuning.driver_interval {
            let Some(runnable) = self.next() else { break };
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
        self.count_drive_from(0, self.tuning.driver_interval);
    }

    /// A turn of the run loop that ran no task and looked at no driver: the future woke itself
    /// again, or an idle pass asked no poller (its park backed out with the spin cold). The count
    /// of runs stays where it was, and
    /// every `driver_interval`th such turn looks without waiting (proofs/tla/loop/RunLoop.tla,
    /// `L_backout`).
    #[inline(always)]
    fn count_unlooked(&self, turns: &mut u32) {
        *turns += 1;
        if *turns >= self.tuning.driver_interval {
            *turns = 0;
            self.count_again(self.since_drive());
            self.drive_or_report(Some(Duration::ZERO));
        }
    }

    /// Runs since timers and the poller were last looked at.
    #[inline]
    pub(super) fn since_drive(&self) -> u32 {
        self.drive_at.get() - self.drive_left.get()
    }

    /// Counts down from `since` runs to the next run that weighs a look: the
    /// next multiple of `QUARTZ_EVERY`, or `interval` (the driver interval)
    /// if it comes first. `since` is below `interval`.
    pub(super) fn count_drive_from(&self, since: u32, interval: u32) {
        let quartz = (since - since % QUARTZ_EVERY).saturating_add(QUARTZ_EVERY);
        let at = quartz.min(interval);
        self.drive_at.set(at);
        self.drive_left.set(at - since);
    }

    /// A run the count stopped at: past the run count, or (at every
    /// `QUARTZ_EVERY`th run) past the time bound since the last look, timers
    /// and the poller get a look, without a wait; a few long runs hold the
    /// driver off as much as many short. Otherwise the count goes on.
    #[inline(never)]
    fn drive_count_stop(&self) {
        let since = self.drive_at.get();
        let interval = self.tuning.driver_interval;
        if since >= interval || (since.is_multiple_of(QUARTZ_EVERY) && self.drive_overdue()) {
            self.count_again(since);
            self.drive_or_report(Some(Duration::ZERO));
        } else {
            self.count_drive_from(since, interval);
        }
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

    /// The shard's new runner, handed it by the monitor, takes its IO over before its first
    /// drive: on a ring, what the old runner armed to wait for readiness, connections or a wake is
    /// armed again from this thread (`ShardRing::adopt`). The other backends' pollers serve any
    /// thread as it is.
    pub(crate) fn adopt(&self) {
        #[cfg(ring)]
        if let Some(ring) = self.ring() {
            ring.adopt(Some(self));
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
