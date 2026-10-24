//! The receiver side of the channel, call by call: `changed`, the version marks,
//! `borrow_and_update`, what a receiver sees when the senders go, and a panicking
//! modifier that publishes nothing.

use super::*;

#[test]
fn single_rx_recv() {
    let (tx, mut rx) = watch::channel("one");

    {
        // Not initially notified.
        let mut t = Task::new(rx.changed());
        t.pending();
    }
    assert_eq!(*rx.borrow(), "one");

    {
        let mut t = Task::new(rx.changed());
        t.pending();
        tx.send("two").unwrap();
        assert!(t.is_woken());
        assert!(t.ready().is_ok());
    }
    assert_eq!(*rx.borrow(), "two");

    {
        let mut t = Task::new(rx.changed());
        t.pending();
        drop(tx);
        assert!(t.is_woken());
        assert!(t.ready().is_err());
    }
    assert_eq!(*rx.borrow(), "two");
}

#[test]
fn rx_version_underflow() {
    let (_tx, mut rx) = watch::channel("one");
    rx.mark_changed();
    rx.mark_changed();
    assert!(rx.has_changed().unwrap());
}

#[test]
fn rx_mark_changed() {
    let (tx, mut rx) = watch::channel("one");

    let mut rx2 = rx.clone();
    let mut rx3 = rx.clone();
    let mut rx4 = rx.clone();
    {
        rx.mark_changed();
        assert!(rx.has_changed().unwrap());
        let mut t = Task::new(rx.changed());
        assert!(t.ready().is_ok());
    }
    {
        assert!(!rx2.has_changed().unwrap());
        let mut t = Task::new(rx2.changed());
        t.pending();
    }
    {
        rx3.mark_changed();
        assert_eq!(*rx3.borrow(), "one");
        assert!(rx3.has_changed().unwrap());
        assert_eq!(*rx3.borrow_and_update(), "one");
        assert!(!rx3.has_changed().unwrap());
        let mut t = Task::new(rx3.changed());
        t.pending();
    }
    {
        tx.send("two").unwrap();
        assert!(rx4.has_changed().unwrap());
        assert_eq!(*rx4.borrow_and_update(), "two");
        rx4.mark_changed();
        assert!(rx4.has_changed().unwrap());
        assert_eq!(*rx4.borrow_and_update(), "two");
    }
    assert_eq!(*rx.borrow(), "two");
}

#[test]
fn rx_mark_unchanged() {
    let (tx, mut rx) = watch::channel("one");
    let mut rx2 = rx.clone();
    {
        assert!(!rx.has_changed().unwrap());
        rx.mark_changed();
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        assert!(!rx.has_changed().unwrap());
        let mut t = Task::new(rx.changed());
        t.pending();
    }
    {
        assert!(!rx2.has_changed().unwrap());
        tx.send("two").unwrap();
        assert!(rx2.has_changed().unwrap());
        rx2.mark_unchanged();
        assert!(!rx2.has_changed().unwrap());
        assert_eq!(*rx2.borrow_and_update(), "two");
    }
    assert_eq!(*rx.borrow(), "two");
}

#[test]
fn multi_rx() {
    let (tx, mut rx1) = watch::channel("one");
    let mut rx2 = rx1.clone();

    {
        let mut t1 = Task::new(rx1.changed());
        let mut t2 = Task::new(rx2.changed());
        t1.pending();
        t2.pending();
    }
    assert_eq!(*rx1.borrow(), "one");
    assert_eq!(*rx2.borrow(), "one");

    let mut t2 = Task::new(rx2.changed());
    {
        let mut t1 = Task::new(rx1.changed());
        t1.pending();
        t2.pending();
        tx.send("two").unwrap();
        assert!(t1.is_woken());
        assert!(t2.is_woken());
        assert!(t1.ready().is_ok());
    }
    assert_eq!(*rx1.borrow(), "two");

    {
        let mut t1 = Task::new(rx1.changed());
        t1.pending();
        tx.send("three").unwrap();
        assert!(t1.is_woken());
        assert!(t2.is_woken());
        assert!(t1.ready().is_ok());
        assert!(t2.ready().is_ok());
    }
    assert_eq!(*rx1.borrow(), "three");

    drop(t2);
    assert_eq!(*rx2.borrow(), "three");

    {
        let mut t1 = Task::new(rx1.changed());
        let mut t2 = Task::new(rx2.changed());
        t1.pending();
        t2.pending();
        tx.send("four").unwrap();
        assert!(t1.ready().is_ok());
        assert!(t2.ready().is_ok());
    }
    assert_eq!(*rx1.borrow(), "four");
    assert_eq!(*rx2.borrow(), "four");
}

#[test]
fn rx_observes_final_value() {
    // Initial value.
    let (tx, mut rx) = watch::channel("one");
    drop(tx);
    {
        let mut t1 = Task::new(rx.changed());
        assert!(t1.ready().is_err());
    }
    assert_eq!(*rx.borrow(), "one");

    // Sending a value.
    let (tx, mut rx) = watch::channel("one");
    tx.send("two").unwrap();
    {
        let mut t1 = Task::new(rx.changed());
        assert!(t1.ready().is_ok());
    }
    assert_eq!(*rx.borrow(), "two");

    {
        let mut t1 = Task::new(rx.changed());
        t1.pending();
        tx.send("three").unwrap();
        drop(tx);
        assert!(t1.is_woken());
        assert!(t1.ready().is_ok());
    }
    assert_eq!(*rx.borrow(), "three");

    {
        let mut t1 = Task::new(rx.changed());
        assert!(t1.ready().is_err());
    }
    assert_eq!(*rx.borrow(), "three");
}

#[test]
fn poll_close() {
    let (tx, rx) = watch::channel("one");
    {
        let mut t = Task::new(tx.closed());
        t.pending();
        drop(rx);
        assert!(t.is_woken());
        t.ready();
    }
    assert!(tx.send("two").is_err());
}

#[test]
fn borrow_and_update() {
    let (tx, mut rx) = watch::channel("one");

    assert!(!rx.has_changed().unwrap());
    tx.send("two").unwrap();
    assert!(rx.has_changed().unwrap());
    assert!(Task::new(rx.changed()).ready().is_ok());
    Task::new(rx.changed()).pending();
    assert!(!rx.has_changed().unwrap());

    tx.send("three").unwrap();
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), "three");
    Task::new(rx.changed()).pending();
    assert!(!rx.has_changed().unwrap());

    drop(tx);
    assert_eq!(*rx.borrow_and_update(), "three");
    assert!(Task::new(rx.changed()).ready().is_err());
    assert!(rx.has_changed().is_err());
}

#[test]
fn reopened_after_subscribe() {
    let (tx, rx) = watch::channel("one");
    assert!(!tx.is_closed());
    drop(rx);
    assert!(tx.is_closed());
    let rx = tx.subscribe();
    assert!(!tx.is_closed());
    drop(rx);
    assert!(tx.is_closed());
}

#[test]
fn a_panicking_modifier_publishes_nothing() {
    let (tx, mut rx) = watch::channel("one");

    tx.send_modify(|old| *old = "two");
    assert_eq!(*rx.borrow_and_update(), "two");

    let mut rx2 = rx.clone();
    assert_eq!(*rx2.borrow_and_update(), "two");
    let mut task = Task::new(rx2.changed());

    let result = catch_unwind(AssertUnwindSafe(|| {
        tx.send_modify(|old| {
            *old = "panicked";
            panic!("the modifier panics");
        })
    }));
    assert!(result.is_err());

    task.pending();
    assert!(!task.is_woken(), "no receiver is notified");
    assert_eq!(*rx.borrow(), "two", "the channel keeps the value it had");
    assert!(!rx.has_changed().unwrap());

    tx.send_modify(|old| *old = "three");
    assert!(task.ready().is_ok());
    assert_eq!(*rx.borrow_and_update(), "three");
}

#[test]
fn multiple_sender() {
    let (tx1, mut rx) = watch::channel(0);
    let tx2 = tx1.clone();

    let mut t = Task::new(async {
        rx.changed().await.unwrap();
        let v1 = *rx.borrow_and_update();
        rx.changed().await.unwrap();
        let v2 = *rx.borrow_and_update();
        (v1, v2)
    });

    tx1.send(1).unwrap();
    t.pending();
    tx2.send(2).unwrap();
    assert_eq!(t.ready(), (1, 2));
}

#[test]
fn receiver_is_notified_when_last_sender_is_dropped() {
    let (tx1, mut rx) = watch::channel(0);
    let tx2 = tx1.clone();

    let mut t = Task::new(rx.changed());
    t.pending();

    drop(tx1);
    assert!(!t.is_woken());
    drop(tx2);
    assert!(t.is_woken());
}

#[test]
fn changed_succeeds_on_closed_channel_with_unseen_value() {
    let (tx, mut rx) = watch::channel("A");
    tx.send("B").unwrap();
    drop(tx);
    block_on(rx.changed()).expect("the current value is not seen");
}

#[test]
fn changed_errors_on_closed_channel_with_seen_value() {
    let (tx, mut rx) = watch::channel("A");
    drop(tx);
    block_on(rx.changed()).expect_err("the channel is closed and the value seen");
}

#[test]
fn has_changed_errors_on_closed_channel() {
    let (tx, rx) = watch::channel("A");
    tx.send("B").unwrap();
    drop(tx);
    rx.has_changed()
        .expect_err("an error if and only if the channel is closed, whatever is seen");

    let (tx, rx) = watch::channel("A");
    drop(tx);
    rx.has_changed().expect_err("closed");
}

#[test]
fn receiver_is_closed_after_all_senders_are_dropped() {
    let (tx, rx) = watch::channel("A");
    let tx2 = tx.clone();
    assert!(!rx.is_closed());
    drop(tx);
    assert!(!rx.is_closed());
    drop(tx2);
    assert!(rx.is_closed());
}
