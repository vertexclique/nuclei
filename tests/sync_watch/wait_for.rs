//! `Receiver::wait_for`: a satisfied predicate is ready at once, an unsatisfied one waits
//! and skips values, a closed channel ends it, and it is cancel safe.

use super::*;

#[test]
fn wait_for_a_true_predicate_on_the_current_value_is_ready_even_if_seen() {
    let (_tx, mut rx) = watch::channel(5);
    assert_eq!(*rx.borrow_and_update(), 5);
    let mut t = Task::new(rx.wait_for(|v| *v == 5));
    let got = t.ready().expect("the current value satisfies it");
    assert_eq!(*got, 5);
    assert!(!got.has_changed(), "it was seen already");
}

#[test]
fn wait_for_waits_and_skips_values_that_do_not_satisfy() {
    let (tx, mut rx) = watch::channel(0);
    let calls = AtomicUsize::new(0);
    {
        let mut t = Task::new(rx.wait_for(|v| {
            calls.fetch_add(1, Ordering::SeqCst);
            *v == 3
        }));
        t.pending();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "judged the current value");
        tx.send(1).unwrap();
        assert!(t.is_woken());
        t.pending();
        tx.send(2).unwrap();
        tx.send(3).unwrap();
        let got = t.ready().expect("3 was sent");
        assert_eq!(*got, 3);
        // As in the established API: the wait already marked the version it woke
        // for, so the returned borrow does not report it as changed.
        assert!(!got.has_changed());
    }
    assert!(!rx.has_changed().unwrap(), "the satisfying value is seen");
}

#[test]
fn wait_for_judges_only_the_newest_of_several_sends() {
    let (tx, mut rx) = watch::channel(0);
    let seen = std::sync::Mutex::new(Vec::new());
    let mut t = Task::new(rx.wait_for(|v| {
        seen.lock().unwrap().push(*v);
        *v == 9
    }));
    t.pending();
    for i in 1..=9 {
        tx.send(i).unwrap();
    }
    assert_eq!(*t.ready().expect("9 was sent"), 9);
    assert_eq!(*seen.lock().unwrap(), vec![0, 9], "the burst was coalesced");
}

#[test]
fn wait_for_errors_on_a_closed_channel_when_the_last_value_fails_the_predicate() {
    let (tx, mut rx) = watch::channel("A");
    tx.send("B").unwrap();
    drop(tx);
    let calls = AtomicUsize::new(0);
    let result = block_on(rx.wait_for(|v| {
        calls.fetch_add(1, Ordering::SeqCst);
        *v == "never"
    }));
    assert!(result.is_err());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "judged the last value once"
    );
}

#[test]
fn wait_for_succeeds_on_a_closed_channel_when_the_last_value_passes() {
    let (tx, mut rx) = watch::channel("A");
    tx.send("B").unwrap();
    drop(tx);
    block_on(rx.wait_for(|_| true)).expect("the last value satisfied it");
}

#[test]
fn wait_for_is_woken_by_the_close() {
    let (tx, mut rx) = watch::channel(1);
    let mut t = Task::new(rx.wait_for(|v| *v == 2));
    t.pending();
    drop(tx);
    assert!(t.is_woken());
    assert!(t.ready().is_err());
}

#[test]
fn wait_for_is_cancel_safe() {
    let (tx, mut rx) = watch::channel(0);
    {
        let mut t = Task::new(rx.wait_for(|v| *v == 2));
        t.pending();
        tx.send(2).unwrap();
        // Dropped without being polled again: the value 2 was never judged.
    }
    assert!(
        rx.has_changed().unwrap() || *rx.borrow() == 2,
        "the satisfying value is still there"
    );
    let mut t = Task::new(rx.wait_for(|v| *v == 2));
    assert_eq!(*t.ready().expect("sent"), 2);
}

#[test]
fn a_panic_in_the_wait_for_closure_poisons_nothing() {
    let (tx, mut rx) = watch::channel(0);
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut t = Task::new(rx.wait_for(|_| panic!("the predicate panics")));
        let _ = t.poll();
    }));
    assert!(result.is_err());
    tx.send(1).unwrap();
    assert_eq!(*rx.borrow(), 1);
    assert_eq!(*block_on(rx.wait_for(|v| *v == 1)).unwrap(), 1);
}
