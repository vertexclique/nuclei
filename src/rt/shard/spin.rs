//! How long a shard spins before it parks: [`Spin`].

use core::cell::Cell;
use std::time::{Duration, Instant};

/// The longest a shard spins before it parks, in nanoseconds.
const SPIN_MAX: u32 = 16_000;
/// The spin a fresh shard starts with.
const SPIN_START: u32 = 4_000;
/// The spin a shard whose park was cut short starts again with.
const SPIN_RESTART: u32 = 1_000;
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
/// ended soon after it began sets it going again. The budget is time, read
/// from the clock after each gap of pauses (never every pause), and is a
/// hint: it only sizes the next spin.
///
/// A shard spins only while work may come soon: its caller says so (a job of
/// its runtime's blocking pool, finite operations of its ring out), or the
/// shard has seen work come from another thread within a spin or soon after
/// a park (`hot`) and nothing since has said otherwise. Else it parks at
/// once and the budget stays.
pub(crate) struct Spin {
    budget: Cell<u32>,
    hot: Cell<bool>,
}

impl Spin {
    pub(super) fn new() -> Self {
        Self {
            budget: Cell::new(SPIN_START),
            hot: Cell::new(false),
        }
    }

    /// With `expecting` work, spins until `found` says it is there (true) or
    /// the budget is spent (false).
    pub(super) fn run(&self, expecting: bool, mut found: impl FnMut() -> bool) -> bool {
        let budget = self.budget.get();
        if !(expecting || self.hot.get()) || budget == 0 {
            return false;
        }
        let limit = Duration::from_nanos(u64::from(budget));
        let start = Instant::now();
        let mut gap = 1;
        loop {
            for _ in 0..gap {
                core::hint::spin_loop();
            }
            gap = (gap * 2).min(SPIN_GAP_MAX);
            if found() {
                self.budget.set((budget * 2).clamp(SPIN_RESTART, SPIN_MAX));
                self.hot.set(true);
                return true;
            }
            if start.elapsed() >= limit {
                break;
            }
        }
        self.budget.set(budget / 2);
        self.hot.set(false);
        false
    }

    /// The shard slept for `slept`: a short sleep means work came right
    /// after the spin gave up.
    pub(super) fn parked_for(&self, slept: Duration) {
        if slept < SHORT_PARK {
            self.hot.set(true);
            self.budget.set(self.budget.get().max(SPIN_RESTART));
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::time::Duration;

    use super::{SPIN_MAX, SPIN_RESTART, SPIN_START, Spin};

    #[test]
    fn a_spin_that_finds_work_doubles_the_budget_and_one_that_runs_out_halves_it() {
        let spin = Spin::new();
        assert_eq!(spin.budget.get(), SPIN_START);
        assert!(spin.run(true, || true));
        assert_eq!(spin.budget.get(), SPIN_START * 2);
        assert!(!spin.run(true, || false));
        assert_eq!(spin.budget.get(), SPIN_START);
        for _ in 0..20 {
            spin.run(true, || true);
        }
        assert_eq!(spin.budget.get(), SPIN_MAX, "capped");
        for _ in 0..20 {
            spin.run(true, || false);
        }
        assert_eq!(spin.budget.get(), 0, "an idle shard stops spinning");
        assert!(!spin.run(true, || true), "a spent budget does not spin");
    }

    #[test]
    fn a_shard_expecting_nothing_parks_at_once_and_keeps_its_budget() {
        let spin = Spin::new();
        assert!(!spin.hot.get());
        let mut looks = 0;
        assert!(!spin.run(false, || {
            looks += 1;
            true
        }));
        assert_eq!(looks, 0, "it never looked");
        assert_eq!(spin.budget.get(), SPIN_START);
        spin.parked_for(Duration::from_micros(3));
        assert!(spin.hot.get(), "a short park: work comes soon after one");
        assert!(spin.run(false, || true), "so it spins unasked");
        assert!(!spin.run(false, || false));
        assert!(!spin.hot.get(), "a spin that ran out ends it");
        assert!(!spin.run(false, || true));
    }

    #[test]
    fn a_spin_ends_by_time_not_by_looks() {
        let spin = Spin::new();
        let start = std::time::Instant::now();
        assert!(!spin.run(true, || false));
        let spent = start.elapsed();
        assert!(
            spent >= Duration::from_nanos(u64::from(SPIN_START)),
            "{spent:?}"
        );
        assert!(spent < Duration::from_millis(50), "{spent:?}");
    }

    #[test]
    fn a_short_park_starts_the_spin_again() {
        let spin = Spin::new();
        for _ in 0..20 {
            spin.run(true, || false);
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
