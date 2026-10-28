//! How long a shard spins before it parks: [`Spin`].

use core::cell::Cell;
use std::time::Duration;

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
    pub(super) fn new() -> Self {
        Self {
            budget: Cell::new(SPIN_START),
        }
    }

    /// Spins until `found` says work is there (true) or the budget is spent
    /// (false).
    pub(super) fn run(&self, mut found: impl FnMut() -> bool) -> bool {
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
    pub(super) fn parked_for(&self, slept: Duration) {
        if slept < SHORT_PARK {
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
