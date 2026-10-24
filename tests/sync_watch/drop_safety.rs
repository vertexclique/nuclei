//! Drop safety: every value is dropped exactly once and never while still borrowed, and a
//! modification that declines drops its copy.

use super::*;

#[test]
fn the_value_is_dropped_when_the_last_handle_goes() {
    let drops = Drops::new();
    let (tx, rx) = watch::channel(drops.value());
    let tx2 = tx.clone();
    let rx2 = rx.clone();
    drop((tx, rx));
    assert_eq!(drops.count(), 0, "the channel still has handles");
    drop(tx2);
    assert_eq!(drops.count(), 0, "a receiver remains");
    drop(rx2);
    eventually("the value was dropped", || drops.count() == 1);
    assert_eq!(drops.count(), 1, "exactly once");
}

#[test]
fn every_replaced_value_is_dropped_exactly_once() {
    let drops = Drops::new();
    let (tx, rx) = watch::channel(drops.value());
    for _ in 0..500 {
        tx.send(drops.value()).unwrap();
    }
    drop((tx, rx));
    eventually("all 501 values were dropped", || drops.count() == 501);
    assert_eq!(drops.count(), 501);
}

#[test]
fn a_replaced_value_that_is_still_borrowed_is_not_dropped_early() {
    let first = Drops::new();
    let rest = Drops::new();
    let (tx, rx) = watch::channel(first.value());
    let held = rx.borrow();
    for _ in 0..500 {
        tx.send(rest.value()).unwrap();
        kovan::flush();
    }
    assert_eq!(first.count(), 0, "a borrow keeps the value it points to");
    drop(held);
    eventually("the first value was dropped once the borrow ended", || {
        // More replacements move the reclamation along.
        tx.send(rest.value()).unwrap();
        first.count() == 1
    });
    assert_eq!(first.count(), 1);
}

#[test]
fn send_replace_hands_back_a_clone_and_drops_the_original_later() {
    let drops = Drops::new();
    let (tx, rx) = watch::channel(drops.value());
    let old: Counted = tx.send_replace(drops.value());
    drop(old);
    assert!(drops.count() >= 1, "the returned clone is the caller's");
    drop((tx, rx));
    // The initial value, its clone and the replacement.
    eventually("everything was dropped", || drops.count() == 3);
    assert_eq!(drops.count(), 3);
}

#[test]
fn a_modifier_that_declines_drops_its_copy() {
    let drops = Drops::new();
    let (tx, rx) = watch::channel(drops.value());
    assert!(!tx.send_if_modified(|_| false));
    assert_eq!(drops.count(), 1, "the unpublished copy was dropped");
    drop((tx, rx));
    eventually("the original was dropped", || drops.count() == 2);
}
