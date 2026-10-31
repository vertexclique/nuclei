use nuclei::sync::local::oneshot;
use nuclei::sync::oneshot::error::TryRecvError;

use crate::sync_support::{Drops, Task};
use crate::util::now;

#[test]
fn a_value_sent_first_is_received() {
    let (tx, rx) = oneshot::channel();
    tx.send(5).unwrap();
    assert_eq!(now(rx), Ok(5));
}

#[test]
fn a_pending_receive_is_woken_by_the_send() {
    let (tx, rx) = oneshot::channel();
    let mut recv = Task::new(rx);
    recv.pending();
    assert_eq!(recv.wakes(), 0);
    tx.send(7).unwrap();
    assert_eq!(recv.wakes(), 1);
    assert_eq!(recv.ready(), Ok(7));
}

#[test]
fn a_dropped_sender_ends_the_receive_with_an_error() {
    let (tx, rx) = oneshot::channel::<u8>();
    let mut recv = Task::new(rx);
    recv.pending();
    drop(tx);
    assert_eq!(recv.wakes(), 1);
    assert!(recv.ready().is_err());
}

#[test]
fn a_send_to_a_closed_receiver_gives_the_value_back() {
    let (tx, mut rx) = oneshot::channel();
    assert!(!tx.is_closed());
    rx.close();
    assert!(tx.is_closed());
    assert_eq!(tx.send(3), Err(3));
    assert!(rx.is_empty());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    assert!(rx.is_terminated());
}

#[test]
fn a_send_to_a_dropped_receiver_gives_the_value_back() {
    let (tx, rx) = oneshot::channel();
    drop(rx);
    assert_eq!(tx.send(3), Err(3));
}

#[test]
fn a_value_sent_before_the_close_stays_receivable() {
    let (tx, mut rx) = oneshot::channel();
    tx.send(9).unwrap();
    rx.close();
    assert!(!rx.is_empty());
    assert_eq!(now(rx), Ok(9));
}

#[test]
fn try_recv_walks_through_its_states() {
    let (tx, mut rx) = oneshot::channel();
    assert!(rx.is_empty());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    assert!(!rx.is_terminated());
    tx.send(1).unwrap();
    assert!(!rx.is_empty());
    assert_eq!(rx.try_recv(), Ok(1));
    assert!(rx.is_terminated());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
}

#[test]
fn try_recv_after_the_sender_dropped_is_closed() {
    let (tx, mut rx) = oneshot::channel::<u8>();
    drop(tx);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    assert!(rx.is_terminated());
}

#[test]
fn closed_completes_when_the_receiver_closes_or_drops() {
    let (mut tx, mut rx) = oneshot::channel::<u8>();
    let mut closed = Task::new(tx.closed());
    closed.pending();
    rx.close();
    assert_eq!(closed.wakes(), 1);
    closed.ready();
    drop(closed);
    now(tx.closed());
    assert_eq!(tx.send(1), Err(1));

    let (mut tx, rx) = oneshot::channel::<u8>();
    let mut closed = Task::new(tx.closed());
    closed.pending();
    drop(rx);
    assert_eq!(closed.wakes(), 1);
    closed.ready();
}

#[test]
fn poll_closed_registers_the_latest_waker() {
    let (mut tx, mut rx) = oneshot::channel::<u8>();
    let (mut old, mut new) = (
        crate::sync_support::Probe::new(),
        crate::sync_support::Probe::new(),
    );
    assert!(old.enter(|cx| tx.poll_closed(cx)).is_pending());
    assert!(new.enter(|cx| tx.poll_closed(cx)).is_pending());
    rx.close();
    assert_eq!((old.wakes(), new.wakes()), (0, 1));
    assert!(new.enter(|cx| tx.poll_closed(cx)).is_ready());
}

#[test]
fn an_unreceived_value_is_dropped_by_the_receiver_once() {
    let drops = Drops::new();
    let (tx, rx) = oneshot::channel();
    assert!(tx.send(drops.value()).is_ok());
    assert_eq!(drops.count(), 0);
    drop(rx);
    assert_eq!(drops.count(), 1);
}

#[test]
fn a_received_value_is_not_dropped_by_the_channel() {
    let drops = Drops::new();
    let (tx, rx) = oneshot::channel();
    assert!(tx.send(drops.value()).is_ok());
    let value = now(rx).unwrap();
    assert_eq!(drops.count(), 0);
    drop(value);
    assert_eq!(drops.count(), 1);
}

#[test]
fn a_failed_send_returns_the_value_undropped() {
    let drops = Drops::new();
    let (tx, rx) = oneshot::channel();
    drop(rx);
    let back = tx.send(drops.value()).unwrap_err();
    assert_eq!(drops.count(), 0);
    drop(back);
    assert_eq!(drops.count(), 1);
}

#[test]
#[should_panic(expected = "called after complete")]
fn polling_a_completed_receiver_panics() {
    let (tx, rx) = oneshot::channel();
    tx.send(1).unwrap();
    let mut rx = Task::new(rx);
    assert_eq!(rx.ready(), Ok(1));
    let _ = rx.poll();
}

#[test]
fn debug_shows_the_state() {
    let (tx, rx) = oneshot::channel::<u8>();
    assert!(format!("{tx:?}").contains("Sender"));
    assert!(format!("{rx:?}").contains("Receiver"));
}
