//! The shard's timers: the millisecond wheel, the microsecond wheel of the
//! precise sleeps, their times on the runtime's clock, firing what came due,
//! the wait's high-resolution timer for a precise deadline, and the park
//! path's part in a paused clock (publishing the earliest deadline, trying
//! the jump, waiting for one in progress).

use core::cell::RefCell;
use std::time::{Duration, Instant};

use super::Shard;
use crate::sync::internal::Ordering;
use crate::time::wheel::Wheel;

impl Shard {
    /// The shard's millisecond wheel.
    #[cfg(test)]
    pub(crate) fn wheel(&self) -> &RefCell<Wheel> {
        &self.wheel
    }

    /// The shard's millisecond wheel, or its microsecond wheel when
    /// `precise`.
    #[inline]
    pub(crate) fn wheel_of(&self, precise: bool) -> &RefCell<Wheel> {
        if precise { &self.precise } else { &self.wheel }
    }

    /// The deadline `at` in the time of the wheel `precise` picks.
    #[inline]
    pub(crate) fn time_of(&self, precise: bool, at: Instant) -> u64 {
        if precise {
            self.units_after(at, 1_000)
        } else {
            self.wheel_time(at)
        }
    }

    /// The wheel's time for the deadline `at`, rounded up to its
    /// millisecond: a timer never fires before its deadline.
    pub(crate) fn wheel_time(&self, at: Instant) -> u64 {
        self.units_after(at, 1_000_000)
    }

    /// `at` in units of `unit` nanoseconds after the epoch, rounded up: what
    /// a wheel arms, so its timer never fires before `at`.
    #[inline]
    fn units_after(&self, at: Instant, unit: u128) -> u64 {
        let since = at.saturating_duration_since(self.epoch);
        u64::try_from(since.as_nanos().div_ceil(unit)).unwrap_or(u64::MAX)
    }

    /// The wheel's time now on the runtime's clock, rounded down: the wheel
    /// never runs ahead of the clock.
    pub(super) fn wheel_now(&self) -> u64 {
        let since = crate::time::clock::now().saturating_duration_since(self.epoch);
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    }

    /// The microsecond wheel's time now, rounded down as `wheel_now`.
    fn precise_now(&self) -> u64 {
        let since = crate::time::clock::now().saturating_duration_since(self.epoch);
        u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
    }

    /// The earliest time either wheel has work at, and whether it is the
    /// microsecond wheel's.
    fn next_deadline(&self) -> Option<(Instant, bool)> {
        let coarse = self
            .wheel
            .borrow()
            .next_deadline()
            .map(|ms| self.epoch + Duration::from_millis(ms));
        let precise = self.precise.borrow();
        let fine = if precise.armed() == 0 {
            None
        } else {
            precise
                .next_deadline()
                .map(|us| self.epoch + Duration::from_micros(us))
        };
        match (coarse, fine) {
            (Some(coarse), Some(fine)) if fine < coarse => Some((fine, true)),
            (Some(coarse), _) => Some((coarse, false)),
            (None, fine) => fine.map(|fine| (fine, true)),
        }
    }

    /// Before a wait that may sleep (`parking`) until a precise deadline,
    /// `after` from now: a poller whose own timeout counts milliseconds
    /// (epoll), or lets the system coalesce it (kqueue), arms its
    /// high-resolution timer for it. A ring's timeout and a port's timer are
    /// high-resolution already: their wait ends there by itself.
    fn arm_precise(&self, parking: bool, next: Option<(Instant, bool)>, after: Option<Duration>) {
        #[cfg(readiness)]
        if parking
            && let (Some((at, true)), Some(after)) = (next, after)
            && let super::Driver::Poll(poller) = &self.driver
        {
            poller.borrow_mut().arm_precise(at, after);
        }
        #[cfg(not(readiness))]
        let _ = (parking, next, after);
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
        // deadline's millisecond (or microsecond) and not one early.
        let earliest = self.next_deadline();
        let next = earliest.map(|(at, _)| at);
        if !crate::time::clock::any_altered() {
            let wait = next.map(|at| at.saturating_duration_since(Instant::now()));
            self.arm_precise(parking, earliest, wait);
            return wait;
        }
        let Some(inner) = crate::rt::context::runtime() else {
            let wait = next.map(|at| at.saturating_duration_since(crate::time::clock::now()));
            self.arm_precise(parking, earliest, wait);
            return wait;
        };
        let now = inner.clock.now();
        if !inner.clock.is_paused() {
            let wait = next.map(|at| at.saturating_duration_since(now));
            self.arm_precise(parking, earliest, wait);
            return wait;
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
            let at = self
                .next_deadline()
                .map_or(u64::MAX, |(at, _)| crate::time::clock::to_nanos(at));
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
        if self.precise.borrow().armed() != 0 {
            self.fire_precise();
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

    /// Fires the precise timers that came due, as `fire_timers` does the
    /// wheel's: out of line, as most shards never arm one.
    #[inline(never)]
    fn fire_precise(&self) {
        let now = self.precise_now();
        let mut fired = self.fired.take();
        self.precise.borrow_mut().advance(now, &mut fired);
        for waker in fired.drain(..) {
            waker.wake();
        }
        self.fired.replace(fired);
    }
}
