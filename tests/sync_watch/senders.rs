//! The `Ref` guard and the sender side: the `send` family and what each returns,
//! counts, subscription and cloning, and the `Display` and `Debug` output.

use super::*;

#[test]
fn ref_has_changed_follows_the_receivers_view() {
    let (tx, mut rx) = watch::channel("hello");
    assert!(!tx.borrow().has_changed(), "never for the sender");
    tx.send("goodbye").unwrap();
    assert!(!tx.borrow().has_changed());
    assert!(rx.borrow().has_changed());
    assert!(rx.borrow().has_changed(), "borrow does not mark seen");
    assert!(rx.borrow_and_update().has_changed());
    assert!(!rx.borrow().has_changed());
    drop(tx);
    assert!(
        !rx.borrow().has_changed(),
        "a closed channel is not an error here"
    );
}

#[test]
fn a_ref_keeps_its_value_while_the_sender_replaces_it() {
    let (tx, rx) = watch::channel(String::from("first"));
    let held = rx.borrow();
    for i in 0..200 {
        tx.send(format!("value {i}")).unwrap();
    }
    assert_eq!(*held, "first", "the held reference is not invalidated");
    assert_eq!(held.len(), 5);
    drop(held);
    assert_eq!(*rx.borrow(), "value 199");
}

#[test]
fn send_with_no_receiver_fails_and_returns_the_value() {
    let (tx, rx) = watch::channel(1);
    drop(rx);
    let SendError(back) = tx.send(2).unwrap_err();
    assert_eq!(back, 2);
    assert_eq!(*tx.borrow(), 1, "a failed send publishes nothing");
}

#[test]
fn sender_new_starts_without_receivers() {
    let sender = watch::Sender::new(0u8);
    assert!(sender.is_closed());
    assert!(sender.send(3).is_err());
    let rx = sender.subscribe();
    assert!(sender.send(4).is_ok());
    assert_eq!(*rx.borrow(), 4);
}

#[test]
fn sender_default_holds_the_default_value() {
    let sender: watch::Sender<u32> = watch::Sender::default();
    assert_eq!(*sender.borrow(), 0);
}

#[test]
fn the_non_failing_senders_work_with_no_receiver() {
    let (tx, rx) = watch::channel(1);
    drop(rx);
    assert_eq!(tx.send_replace(2), 1);
    tx.send_modify(|v| *v += 1);
    assert!(tx.send_if_modified(|v| {
        *v += 1;
        true
    }));
    assert_eq!(*tx.borrow(), 4);
    let rx = tx.subscribe();
    assert_eq!(*rx.borrow(), 4, "a late subscriber sees the latest value");
}

#[test]
fn send_replace_returns_the_previous_value_and_notifies() {
    let (tx, mut rx) = watch::channel(1);
    assert_eq!(tx.send_replace(2), 1);
    assert_eq!(tx.send_replace(3), 2);
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), 3);
}

#[test]
fn send_modify_notifies_even_when_the_value_is_unchanged() {
    let (tx, rx) = watch::channel(1);
    tx.send_modify(|_| {});
    assert!(rx.has_changed().unwrap());
}

#[test]
fn send_if_modified_notifies_only_when_the_closure_says_so() {
    let (tx, mut rx) = watch::channel(1u32);
    let inc_if_odd = |n: &mut u32| {
        if *n % 2 == 1 {
            *n += 1;
            return true;
        }
        false
    };
    assert!(!rx.has_changed().unwrap());
    assert!(tx.send_if_modified(inc_if_odd));
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), 2);
    assert!(!tx.send_if_modified(inc_if_odd));
    assert!(!rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), 2);
}

#[test]
fn a_false_from_send_if_modified_discards_what_the_closure_did() {
    let (tx, rx) = watch::channel(1u32);
    assert!(!tx.send_if_modified(|n| {
        *n = 99;
        false
    }));
    assert_eq!(*rx.borrow(), 1, "no silent modification");
}

#[test]
fn counts_and_identity() {
    let (tx, rx) = watch::channel("hello");
    assert_eq!((tx.sender_count(), tx.receiver_count()), (1, 1));
    let rx2 = rx.clone();
    let tx2 = tx.clone();
    assert_eq!((tx.sender_count(), tx.receiver_count()), (2, 2));
    assert!(tx.same_channel(&tx2));
    assert!(rx.same_channel(&rx2));
    let (tx3, rx3) = watch::channel("hello");
    assert!(!tx3.same_channel(&tx2));
    assert!(!rx3.same_channel(&rx2));
    drop((tx2, rx2));
    assert_eq!((tx.sender_count(), tx.receiver_count()), (1, 1));
}

#[test]
fn a_cloned_receiver_inherits_what_the_original_has_seen() {
    let (tx, mut rx) = watch::channel(0);
    tx.send(1).unwrap();
    let unseen = rx.clone();
    assert!(unseen.has_changed().unwrap());
    rx.mark_unchanged();
    let seen = rx.clone();
    assert!(!seen.has_changed().unwrap());
}

#[test]
fn subscribe_marks_the_current_value_seen() {
    let (tx, _rx) = watch::channel(0u64);
    tx.send(5).unwrap();
    let mut rx = tx.subscribe();
    assert_eq!(*rx.borrow(), 5);
    assert!(!rx.has_changed().unwrap());
    Task::new(rx.changed()).pending();
}

#[test]
fn errors_display_debug_and_compare_like_the_established_api() {
    let (tx, rx) = watch::channel(1);
    drop(rx);
    let error = tx.send(7).unwrap_err();
    assert_eq!(error.to_string(), "channel closed");
    assert_eq!(
        format!("{error:?}"),
        "SendError { .. }",
        "the value is not printed"
    );
    assert_eq!(error, SendError(7));
    let copy = error;
    assert_eq!(copy.0, 7);
    let as_error: &dyn Error = &error;
    assert!(as_error.source().is_none());

    let (tx, mut rx) = watch::channel(1);
    drop(tx);
    let recv: RecvError = block_on(rx.changed()).unwrap_err();
    assert_eq!(recv.to_string(), "channel closed");
    assert!(format!("{recv:?}").starts_with("RecvError"));
    let cloned = recv.clone();
    assert_eq!(cloned.to_string(), recv.to_string());
    let as_error: &dyn Error = &recv;
    assert!(as_error.source().is_none());
}

#[test]
fn debug_shows_the_value_version_and_state() {
    let (tx, mut rx) = watch::channel(String::from("x"));
    tx.send(String::from("y")).unwrap();
    let text = format!("{tx:?}");
    assert!(text.contains("Sender") && text.contains("\"y\""), "{text}");
    let text = format!("{rx:?}");
    assert!(
        text.contains("Receiver") && text.contains("version"),
        "{text}"
    );
    let text = format!("{:?}", rx.borrow_and_update());
    assert!(
        text.contains("Ref") && text.contains("has_changed: true"),
        "{text}"
    );
}
