//! The drain role: models that count passes, the first attempt and the spins, each with its calibration.

use super::*;

#[test]
fn a_waiter_serving_the_queue_is_served_while_the_holder_barges() {
    crate::sync::loom_model::a_waiter_serving_the_queue_is_served_while_the_holder_barges();
}

#[test]
#[should_panic(expected = "the drain ran")]
fn calibration_a_marker_lowered_on_a_pending_nudge_keeps_the_drain_passing() {
    crate::sync::loom_model::calibration_a_marker_lowered_on_a_pending_nudge_keeps_the_drain_passing();
}

/// The Mutex's inline first attempt never takes a lock owed to a queued
/// waiter, with the spins; its calibration drops the attempt's HANDOFF test
/// and must fail.
#[test]
fn the_first_attempt_refuses_a_lock_owed_to_a_waiter() {
    crate::sync::loom_model::the_first_attempt_refuses_a_lock_owed_to_a_waiter();
}

#[test]
#[should_panic(expected = "the holder took a lock owed to the queued waiter")]
fn calibration_a_first_attempt_that_ignores_handoff_barges_past_an_owed_waiter() {
    crate::sync::loom_model::calibration_a_first_attempt_that_ignores_handoff_barges_past_an_owed_waiter();
}

/// A release that nudges a waiter spinning on its node calls no waker, and
/// the waiter is served all the same (no lost wakeup), with floors on the
/// spins it must reach; the calibration is the production spin with its look
/// taken away and must leave the waiter armed beside an unread mark.
#[test]
fn a_nudge_while_the_waiter_spins_is_never_lost() {
    crate::sync::loom_model::a_nudge_while_the_waiter_spins_is_never_lost();
}

#[test]
#[should_panic(expected = "the waiter armed and slept beside an unread mark")]
fn calibration_a_node_spin_that_skips_its_look_sleeps_beside_the_mark() {
    crate::sync::loom_model::calibration_a_node_spin_that_skips_its_look_sleeps_beside_the_mark();
}

#[test]
fn a_drain_that_found_nobody_queued_ends_while_another_thread_releases() {
    crate::sync::loom_model::a_drain_that_found_nobody_queued_ends_while_another_thread_releases();
}

#[test]
#[should_panic(expected = "over a queue with nobody in it")]
fn calibration_a_release_that_dirties_keeps_an_empty_drain_passing() {
    crate::sync::loom_model::calibration_a_release_that_dirties_keeps_an_empty_drain_passing();
}

#[test]
#[should_panic(expected = "Causality violation")]
fn calibration_an_unguarded_write_is_caught() {
    explore("calibration_an_unguarded_write_is_caught", 3, 1, || {
        let cell = Arc::new(UnsafeCell::new(0usize));
        let other = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || bump(&cell))
        };
        bump(&cell);
        other.join().expect("other");
    });
}
