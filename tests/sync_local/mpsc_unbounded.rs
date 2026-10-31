use std::task::Poll;

use nuclei::sync::local::mpsc;
use nuclei::sync::mpsc::error::{SendError, TryRecvError};

use crate::sync_support::{Drops, Probe, Task};
use crate::util::now;

#[test]
fn values_come_out_in_the_order_they_went_in() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    for n in 0..100 {
        tx.send(n).unwrap();
    }
    assert_eq!((rx.len(), rx.is_empty()), (100, false));
    for n in 0..100 {
        assert_eq!(now(rx.recv()), Some(n));
    }
    assert!(rx.is_empty());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn a_send_after_close_or_drop_gives_the_value_back() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send(1).unwrap();
    rx.close();
    assert!(rx.is_closed());
    assert!(tx.is_closed());
    assert_eq!(tx.send(2), Err(SendError(2)));
    assert_eq!(rx.try_recv(), Ok(1));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    drop(rx);
    assert_eq!(tx.send(3), Err(SendError(3)));
}

#[test]
fn the_stream_ends_when_the_last_sender_goes() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let other = tx.clone();
    assert!(tx.same_channel(&other));
    tx.send(1).unwrap();
    drop(tx);
    assert_eq!(now(rx.recv()), Some(1));
    let mut probe = Probe::new();
    assert!(probe.enter(|cx| rx.poll_recv(cx)).is_pending());
    drop(other);
    assert_eq!(probe.wakes(), 1);
    assert_eq!(probe.enter(|cx| rx.poll_recv(cx)), Poll::Ready(None));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn a_pending_receive_is_woken_once_by_the_send() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut recv = Task::new(rx.recv());
    recv.pending();
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    assert_eq!(recv.wakes(), 1);
    assert_eq!(recv.ready(), Some(1));
}

#[test]
fn recv_many_takes_what_is_ready() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    for n in 0..5 {
        tx.send(n).unwrap();
    }
    let mut buf = Vec::new();
    assert_eq!(now(rx.recv_many(&mut buf, 3)), 3);
    assert_eq!(now(rx.recv_many(&mut buf, 3)), 2);
    assert_eq!(now(rx.recv_many(&mut buf, 0)), 0);
    assert_eq!(buf, [0, 1, 2, 3, 4]);
    drop(tx);
    assert_eq!(now(rx.recv_many(&mut buf, 3)), 0);
}

#[test]
fn a_pending_recv_many_is_woken_by_the_send() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut probe = Probe::new();
    let mut buf = Vec::new();
    assert!(
        probe
            .enter(|cx| rx.poll_recv_many(cx, &mut buf, 4))
            .is_pending()
    );
    tx.send(1).unwrap();
    assert_eq!(probe.wakes(), 1);
    assert_eq!(
        probe.enter(|cx| rx.poll_recv_many(cx, &mut buf, 4)),
        Poll::Ready(1)
    );
}

#[test]
fn closed_completes_when_the_receiver_closes_or_drops() {
    let (tx, mut rx) = mpsc::unbounded_channel::<u8>();
    let mut closed = Task::new(tx.closed());
    closed.pending();
    rx.close();
    assert_eq!(closed.wakes(), 1);
    closed.ready();
    now(tx.closed());

    let (tx, rx) = mpsc::unbounded_channel::<u8>();
    let mut closed = Task::new(tx.closed());
    closed.pending();
    drop(rx);
    assert_eq!(closed.wakes(), 1);
    closed.ready();
}

#[test]
fn a_dropped_closed_wait_leaves_the_others_intact() {
    let (tx, rx) = mpsc::unbounded_channel::<u8>();
    let mut first = Task::new(tx.closed());
    let mut second = Task::new(tx.closed());
    first.pending();
    second.pending();
    drop(first);
    drop(rx);
    assert_eq!(second.wakes(), 1);
    second.ready();
}

#[test]
fn every_value_is_dropped_exactly_once() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for _ in 0..5 {
        tx.send(drops.value()).unwrap();
    }
    drop(rx.try_recv().unwrap());
    assert_eq!(drops.count(), 1);
    rx.close();
    tx.send(drops.value()).unwrap_err();
    assert_eq!(drops.count(), 2);
    drop(rx);
    assert_eq!(drops.count(), 6);
    drop(tx);
    assert_eq!(drops.count(), 6);
}

#[test]
fn halves_clone_and_debug() {
    let (tx, rx) = mpsc::unbounded_channel::<u8>();
    let other = tx.clone();
    assert!(tx.same_channel(&other));
    assert!(format!("{tx:?}").contains("UnboundedSender"));
    assert!(format!("{rx:?}").contains("UnboundedReceiver"));
}
