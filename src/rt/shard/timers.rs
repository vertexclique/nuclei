//! The shard's timers: the wheel, its time on the runtime's clock, firing
//! what came due, and the park path's part in a paused clock (publishing
//! the earliest deadline, trying the jump, waiting for one in progress).

use core::cell::RefCell;
use std::time::{Duration, Instant};

use super::Shard;
use crate::sync::internal::Ordering;
use crate::time::wheel::Wheel;

impl Shard {
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

    /// The wheel's time now on the runtime's clock, rounded down: the wheel
    /// never runs ahead of the clock.
    pub(super) fn wheel_now(&self) -> u64 {
        let since = crate::time::clock::now().saturating_duration_since(self.epoch);
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    }

    /// How long the shard's wait may last for its next timer, if it has one.
    ///
    /// On a paused clock the timer's wait is not real time: a due timer
    /// fires at once, and otherwise the wait is unbounded, for a waker or for
    /// the clock to move. A shard about to park (`parking`, its deadline
    /// published by `before_park`) tries to move the clock to the runtime's
    /// earliest deadline (`paused::try_advance`); if that makes its own timer
    /// due, it does not wait.
    pub(super) fn timer_wait(&self, parking: bool) -> Option<Duration> {
        // From the clock's own nanoseconds, so the wait ends at the
        // deadline's millisecond and not one early.
        let next = self
            .wheel
            .borrow()
            .next_deadline()
            .map(|ms| self.epoch + Duration::from_millis(ms));
        if !crate::time::clock::any_altered() {
            return next.map(|at| at.saturating_duration_since(Instant::now()));
        }
        let Some(inner) = crate::rt::context::runtime() else {
            return next.map(|at| at.saturating_duration_since(crate::time::clock::now()));
        };
        let now = inner.clock.now();
        if !inner.clock.is_paused() {
            return next.map(|at| at.saturating_duration_since(now));
        }
        if next.is_some_and(|at| at <= now) {
            return Some(Duration::ZERO);
        }
        if !parking {
            return None;
        }
        if crate::rt::paused::try_advance(&inner, Some(&self.shared)) {
            Some(Duration::ZERO)
        } else {
            None
        }
    }

    /// Before a park, for a paused clock: publishes the shard's earliest
    /// deadline and counts the park, both before the shard says it parks, so
    /// a shard deciding a jump that sees this one parked sees its deadline
    /// (`proofs/tla/clock/`, publish_late). Nothing to do while no clock is
    /// paused.
    pub(super) fn before_park(&self) {
        if crate::time::clock::any_altered() {
            let at = self.wheel.borrow().next_deadline().map_or(u64::MAX, |ms| {
                crate::time::clock::to_nanos(self.epoch + Duration::from_millis(ms))
            });
            self.shared.timer_at.store(at, Ordering::SeqCst);
            self.shared.parks.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// After a park, slept or given up for work that came: a jump of a paused
    /// clock in progress ends before this shard runs and reads the clock
    /// (`proofs/tla/clock/`, busy_no_wait and no_wait_advance).
    pub(super) fn after_park(&self) {
        if crate::time::clock::any_altered()
            && let Some(inner) = crate::rt::context::runtime()
        {
            inner.clock.wait_advance();
        }
    }

    /// Fires the due timers from inside a task's poll (`time::advance`): the
    /// tasks they wake go to the queue in deadline order, ahead of a task
    /// that yields next, and not to the LIFO slot.
    pub(crate) fn fire_timers_now(&self) {
        crate::rt::context::with_in_task(false, || self.fire_timers());
    }

    pub(super) fn fire_timers(&self) {
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
}
