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
