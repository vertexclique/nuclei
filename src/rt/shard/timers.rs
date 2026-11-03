//! The shard's timers: the millisecond wheel, the microsecond wheel of the
//! precise sleeps, their times on the runtime's clock, firing what came due,
//! the wait's high-resolution timer for a precise deadline, and the park
//! path's part in a paused clock (publishing the earliest deadline, trying
//! the jump, waiting for one in progress).

use core::cell::{Cell, RefCell};
use core::task::Waker;
use std::time::{Duration, Instant};
use std::vec::Drain;

use super::Shard;
use crate::sync::internal::Ordering;
use crate::time::units::{MICROS, MILLIS, units};
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

    /// The deadline `at` (nanoseconds after the epoch, the clock's origin)
    /// in the time of the wheel `precise` picks, rounded up: a timer never
    /// fires before its deadline.
    #[inline]
    pub(crate) fn time_of(&self, precise: bool, at: u64) -> u64 {
        let per_second = if precise { MICROS } else { MILLIS };
        units(at, per_second, true)
    }

    /// The wheel's time now on the runtime's clock, rounded down: the wheel
    /// never runs ahead of the clock.
    pub(super) fn wheel_now(&self) -> u64 {
        units(crate::time::clock::now_nanos(), MILLIS, false)
    }

    /// The microsecond wheel's time now, rounded down as `wheel_now`.
    pub(super) fn precise_now(&self) -> u64 {
        units(crate::time::clock::now_nanos(), MICROS, false)
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
            let own = precise
                .next_deadline()
                .map(|us| self.epoch + Duration::from_micros(us));
            // Armed here, or counted for the wheels kept aside (`foreign`).
            match (own, self.side_deadline()) {
                (Some(own), Some(side)) => Some(own.min(side)),
                (own, side) => own.or(side),
            }
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
            // The poller's own timer counts the system's time, so it is given
            // the deadline as the system sees it: an advance moves that, the
            // deadline on the clock staying, and the timer is armed again
            // (proofs/tla/precise, `M_key_on_clock`).
            let system = earliest.zip(wait).map(|((at, precise), wait)| {
                let at = inner
                    .clock
                    .system_instant(at)
                    .unwrap_or_else(|| Instant::now() + wait);
                (at, precise)
            });
            self.arm_precise(parking, system, wait);
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
    ///
    /// The caller is the one that called the wakers: a waker that panics
    /// does so out of this call, after every fired timer was woken.
    pub(crate) fn fire_timers_now(&self) {
        crate::rt::context::with_in_task(false, || {
            if let Some(panic) = crate::rt::caught::catch(|| self.fire_armed_timers()) {
                // It reaches the caller, so the worker's loop has nothing to log.
                FIRE_PANIC.set(false);
                std::panic::resume_unwind(panic);
            }
        });
    }

    /// Fires the due timers from the run loop. A waker that panics there
    /// unwinds out of the loop after every fired timer was woken; the
    /// worker's run catches it (`take_fire_panic`) and goes on.
    #[inline]
    pub(super) fn fire_timers(&self) {
        if self.shared.timer_ops.is_empty()
            && self.wheel.borrow().armed() == 0
            && self.precise.borrow().armed() == 0
        {
            return;
        }
        self.fire_armed_timers();
    }

    fn fire_armed_timers(&self) {
        if !self.shared.timer_ops.is_empty() {
            self.apply_timer_ops();
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
        self.wake_fired(fired);
    }

    /// Applies the timer changes other threads posted. The wakers they
    /// remove are dropped, and the ones of timers already due are woken,
    /// after the wheel is let go: both are user code.
    fn apply_timer_ops(&self) {
        // The posted changes belong to the wheels of the runtime the shard
        // serves, which are not in place while a nested future of another is
        // polled: they wait for the loop's next look.
        if self.foreign_poll.get() {
            return;
        }
        let mut fired = self.fired.take();
        let mut removed = Vec::new();
        {
            let mut wheel = self.wheel.borrow_mut();
            self.shared
                .timer_ops
                .take_all(|op| removed.extend(op.apply(self, &mut wheel, &mut fired)));
        }
        let dropped = if removed.is_empty() {
            None
        } else {
            crate::rt::caught::catch(|| drop(removed))
        };
        self.wake_fired(fired);
        if let Some(panic) = dropped {
            FIRE_PANIC.set(true);
            std::panic::resume_unwind(panic);
        }
    }

    /// Wakes the fired wakers in order, and keeps the emptied buffer for the
    /// next fire. A waker that panics does not leave the rest unwoken (their
    /// timers are gone from the wheel, so nothing else would wake them): the
    /// guard wakes them and the panic goes on.
    #[inline(always)]
    pub(super) fn wake_fired(&self, mut fired: Vec<Waker>) {
        let mut rest = Firing(fired.drain(..));
        for waker in rest.0.by_ref() {
            waker.wake();
        }
        // Fully drained: the vector is empty and the guard has nothing to do.
        core::mem::forget(rest);
        self.fired.replace(fired);
    }

    /// Fires the precise timers that came due, as `fire_timers` does the
    /// wheel's: out of line, as most shards never arm one.
    #[inline(never)]
    fn fire_precise(&self) {
        let now = self.precise_now();
        let mut fired = self.fired.take();
        self.precise.borrow_mut().advance(now, &mut fired);
        self.wake_fired(fired);
        if !self.side.borrow().is_empty() {
            self.fire_side();
        }
    }
}

thread_local! {
    /// Set when a timer's waker panicked as it fired on this thread's loop.
    static FIRE_PANIC: Cell<bool> = const { Cell::new(false) };
}

/// Whether the panic the worker's run just caught came from a timer's waker
/// firing, which it logs and goes on from; clears the mark.
pub(super) fn take_fire_panic() -> bool {
    FIRE_PANIC.replace(false)
}

/// Whether the mark is set (tests).
#[cfg(test)]
pub(crate) fn fire_panic_set() -> bool {
    FIRE_PANIC.get()
}

/// Guard of a `block_on`, forgotten on its normal path: an unwind out of it
/// reaches a caller, not the worker's run, so the mark of a timer's panic
/// goes with it.
pub(super) struct ClearOnUnwind;

impl Drop for ClearOnUnwind {
    fn drop(&mut self) {
        FIRE_PANIC.set(false);
    }
}

/// The wakers still to wake. Only an unwind drops it with some left (a wake
/// panicked): it wakes them, each guarded against a second panic.
struct Firing<'a>(Drain<'a, Waker>);

impl Drop for Firing<'_> {
    fn drop(&mut self) {
        FIRE_PANIC.set(true);
        for waker in self.0.by_ref() {
            crate::sync::internal::during_unwind(|| waker.wake());
        }
    }
}
