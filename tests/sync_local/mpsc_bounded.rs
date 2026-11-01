use std::task::Poll;

use nuclei::runtime::Runtime;
use nuclei::sync::local::mpsc;
use nuclei::sync::mpsc::error::{SendError, TryRecvError, TrySendError};
use nuclei::task;

use crate::sync_support::{Drops, Probe, Task};
use crate::util::now;

#[test]
fn values_come_out_in_the_order_they_went_in() {
    let (tx, mut rx) = mpsc::channel(8);
    for n in 0..8 {
        now(tx.send(n)).unwrap();
    }
    for n in 0..8 {
        assert_eq!(now(rx.recv()), Some(n));
    }
}

#[test]
#[should_panic(expected = "buffer > 0")]
fn a_zero_buffer_panics() {
    let _ = mpsc::channel::<u8>(0);
}

#[test]
fn capacity_counts_the_free_slots() {
    let (tx, mut rx) = mpsc::channel::<u8>(3);
    assert_eq!((tx.capacity(), tx.max_capacity()), (3, 3));
    assert_eq!((rx.capacity(), rx.max_capacity()), (3, 3));
    tx.try_send(1).unwrap();
    let permit = tx.try_reserve().unwrap();
    assert_eq!((tx.capacity(), rx.capacity(), rx.len()), (1, 1, 1));
    permit.send(2);
    assert_eq!(rx.len(), 2);
    assert!(!rx.is_empty());
    rx.try_recv().unwrap();
    assert_eq!(tx.capacity(), 2);
}

#[test]
fn try_send_and_try_recv_report_why_they_fail() {
    let (tx, mut rx) = mpsc::channel(1);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    tx.try_send(1).unwrap();
    assert!(matches!(tx.try_send(2), Err(TrySendError::Full(2))));
    assert_eq!(rx.try_recv(), Ok(1));
    drop(tx);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn try_send_to_a_closed_channel_gives_the_value_back() {
    let (tx, mut rx) = mpsc::channel(2);
    rx.close();
    assert!(matches!(tx.try_send(5), Err(TrySendError::Closed(5))));
    assert!(matches!(tx.try_reserve(), Err(TrySendError::Closed(()))));
    assert!(tx.is_closed());
    assert_eq!(now(tx.send(6)), Err(SendError(6)));
}

#[test]
fn a_send_on_a_full_channel_waits_for_a_value_to_be_taken() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(1).unwrap();
    let mut send = Task::new(tx.send(2));
    send.pending();
    assert_eq!(send.wakes(), 0);
    assert_eq!(rx.try_recv(), Ok(1));
    assert_eq!(send.wakes(), 1);
    assert_eq!(send.ready(), Ok(()));
    assert_eq!(rx.try_recv(), Ok(2));
}

#[test]
fn waiting_sends_are_served_in_arrival_order_and_try_send_does_not_barge() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(0).unwrap();
    let mut first = Task::new(tx.send(1));
    let mut second = Task::new(tx.send(2));
    first.pending();
    second.pending();
    assert_eq!(rx.try_recv(), Ok(0));
    assert!(matches!(tx.try_send(9), Err(TrySendError::Full(9))));
    assert_eq!((first.wakes(), second.wakes()), (1, 0));
    first.ready().unwrap();
    assert_eq!(rx.try_recv(), Ok(1));
    assert_eq!(second.wakes(), 1);
    second.ready().unwrap();
    assert_eq!(rx.try_recv(), Ok(2));
}

#[test]
fn the_stream_ends_when_the_last_sender_goes() {
    let (tx, mut rx) = mpsc::channel(2);
    let other = tx.clone();
    assert!(tx.same_channel(&other));
    tx.try_send(1).unwrap();
    drop(tx);
    assert!(!rx.is_closed());
    assert_eq!(now(rx.recv()), Some(1));
    let mut probe = Probe::new();
    assert!(probe.enter(|cx| rx.poll_recv(cx)).is_pending());
    drop(other);
    assert_eq!(probe.wakes(), 1);
    assert!(rx.is_closed());
    assert_eq!(probe.enter(|cx| rx.poll_recv(cx)), Poll::Ready(None));
}

#[test]
fn a_pending_receive_is_woken_once_by_the_send() {
    let (tx, mut rx) = mpsc::channel(4);
    let mut probe = Probe::new();
    assert!(probe.enter(|cx| rx.poll_recv(cx)).is_pending());
    tx.try_send(1).unwrap();
    tx.try_send(2).unwrap();
    assert_eq!(probe.wakes(), 1);
    assert_eq!(probe.enter(|cx| rx.poll_recv(cx)), Poll::Ready(Some(1)));
}

#[test]
fn close_stops_sends_but_keeps_buffered_values_and_reserved_slots() {
    let (tx, mut rx) = mpsc::channel(3);
    tx.try_send(1).unwrap();
    let permit = tx.try_reserve().unwrap();
    rx.close();
    assert!(rx.is_closed());
    assert!(matches!(tx.try_send(2), Err(TrySendError::Closed(2))));
    permit.send(3);
    assert_eq!(rx.try_recv(), Ok(1));
    assert_eq!(rx.try_recv(), Ok(3));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(now(rx.recv()), None);
}

#[test]
fn a_receive_waits_for_an_outstanding_permit_after_the_close() {
    let (tx, mut rx) = mpsc::channel::<u8>(1);
    let permit = tx.try_reserve().unwrap();
    rx.close();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    let mut probe = Probe::new();
    assert!(probe.enter(|cx| rx.poll_recv(cx)).is_pending());
    drop(permit);
    assert_eq!(probe.wakes(), 1);
    assert_eq!(probe.enter(|cx| rx.poll_recv(cx)), Poll::Ready(None));
}

#[test]
fn recv_many_takes_what_is_ready_up_to_the_limit() {
    let (tx, mut rx) = mpsc::channel(8);
    for n in 0..5 {
        tx.try_send(n).unwrap();
    }
    let mut buf = Vec::new();
    assert_eq!(now(rx.recv_many(&mut buf, 3)), 3);
    assert_eq!(buf, [0, 1, 2]);
    assert_eq!(tx.capacity(), 6);
    assert_eq!(now(rx.recv_many(&mut buf, 10)), 2);
    assert_eq!(buf, [0, 1, 2, 3, 4]);
    assert_eq!(now(rx.recv_many(&mut buf, 0)), 0);
    drop(tx);
    assert_eq!(now(rx.recv_many(&mut buf, 4)), 0);
}

#[test]
fn recv_many_frees_the_slots_of_waiting_sends() {
    let (tx, mut rx) = mpsc::channel(2);
    tx.try_send(1).unwrap();
    tx.try_send(2).unwrap();
    let mut first = Task::new(tx.send(3));
    let mut second = Task::new(tx.send(4));
    first.pending();
    second.pending();
    let mut buf = Vec::new();
    assert_eq!(now(rx.recv_many(&mut buf, 2)), 2);
    assert_eq!((first.wakes(), second.wakes()), (1, 1));
    first.ready().unwrap();
    second.ready().unwrap();
    assert_eq!(now(rx.recv_many(&mut buf, 2)), 2);
    assert_eq!(buf, [1, 2, 3, 4]);
}

#[test]
fn a_pending_recv_many_is_woken_by_the_send() {
    let (tx, mut rx) = mpsc::channel(2);
    let mut probe = Probe::new();
    let mut buf = Vec::new();
    assert!(
        probe
            .enter(|cx| rx.poll_recv_many(cx, &mut buf, 2))
            .is_pending()
    );
    tx.try_send(1).unwrap();
    assert_eq!(probe.wakes(), 1);
    assert_eq!(
        probe.enter(|cx| rx.poll_recv_many(cx, &mut buf, 2)),
        Poll::Ready(1)
    );
}

#[test]
fn a_reserve_waits_in_line_and_a_permit_sends_or_gives_the_slot_back() {
    let (tx, mut rx) = mpsc::channel(1);
    let permit = tx.try_reserve().unwrap();
    assert!(matches!(tx.try_reserve(), Err(TrySendError::Full(()))));
    let mut reserve = Task::new(tx.reserve());
    reserve.pending();
    drop(permit);
    assert_eq!(reserve.wakes(), 1);
    reserve.ready().unwrap().send(7);
    assert_eq!(rx.try_recv(), Ok(7));
    assert_eq!(tx.capacity(), 1);
}

#[test]
fn a_reserve_on_a_closed_channel_fails() {
    let (tx, rx) = mpsc::channel::<u8>(1);
    drop(rx);
    assert!(now(tx.reserve()).is_err());
}

#[test]
fn closed_completes_when_the_receiver_closes_or_drops() {
    let (tx, mut rx) = mpsc::channel::<u8>(1);
    let mut closed = Task::new(tx.closed());
    closed.pending();
    assert!(!tx.is_closed());
    rx.close();
    assert_eq!(closed.wakes(), 1);
    closed.ready();
    now(tx.closed());

    let (tx, rx) = mpsc::channel::<u8>(1);
    let mut closed = Task::new(tx.closed());
    closed.pending();
    drop(rx);
    assert_eq!(closed.wakes(), 1);
    closed.ready();
}

#[test]
fn every_closed_wait_is_woken_once() {
    let (tx, rx) = mpsc::channel::<u8>(1);
    let mut waits: Vec<_> = (0..4).map(|_| Task::new(tx.closed())).collect();
    waits.iter_mut().for_each(Task::pending);
    drop(rx);
    assert!(waits.iter().all(|w| w.wakes() == 1));
}

#[test]
fn a_pending_send_fails_when_the_receiver_is_dropped() {
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(1).unwrap();
    let mut send = Task::new(tx.send(2));
    send.pending();
    drop(rx);
    assert_eq!(send.wakes(), 1);
    assert_eq!(send.ready(), Err(SendError(2)));
}

#[test]
fn every_value_is_dropped_exactly_once() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::channel(4);
    for _ in 0..4 {
        tx.try_send(drops.value()).unwrap();
    }
    drop(rx.try_recv().unwrap());
    assert_eq!(drops.count(), 1);
    tx.try_send(drops.value()).unwrap();
    tx.try_send(drops.value()).unwrap_err();
    assert_eq!(drops.count(), 2);
    drop(rx);
    assert_eq!(drops.count(), 6);
    drop(tx);
    assert_eq!(drops.count(), 6);
}

#[test]
fn values_left_when_the_receiver_closes_then_drops_are_dropped_once() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::channel(3);
    for _ in 0..3 {
        tx.try_send(drops.value()).unwrap();
    }
    rx.close();
    assert_eq!(drops.count(), 0);
    drop(rx);
    assert_eq!(drops.count(), 3);
    assert_eq!(tx.capacity(), 3);
}

#[test]
fn a_value_sent_through_a_permit_after_the_receiver_dropped_is_dropped_with_the_channel() {
    let drops = Drops::new();
    let (tx, rx) = mpsc::channel(1);
    let permit = tx.try_reserve().unwrap();
    drop(rx);
    permit.send(drops.value());
    assert_eq!(drops.count(), 0);
    drop(tx);
    assert_eq!(drops.count(), 1);
}

#[test]
fn senders_clone_compare_and_debug() {
    let (tx, rx) = mpsc::channel::<u8>(2);
    let other = tx.clone();
    let (unrelated, _rx) = mpsc::channel::<u8>(2);
    assert!(tx.same_channel(&other));
    assert!(!tx.same_channel(&unrelated));
    assert!(format!("{tx:?}").contains("Sender"));
    assert!(format!("{rx:?}").contains("Receiver"));
    assert!(format!("{:?}", tx.try_reserve().unwrap()).contains("Permit"));
}

#[test]
fn a_producer_and_a_consumer_share_a_small_channel() {
    let rt = Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .expect("the runtime starts");
    let sum = rt.block_on(async {
        let (tx, mut rx) = mpsc::channel(2);
        let producer = task::spawn_local(async move {
            for n in 0..1000u64 {
                tx.send(n).await.unwrap();
            }
        });
        let consumer = task::spawn_local(async move {
            let mut next = 0;
            while let Some(n) = rx.recv().await {
                assert_eq!(n, next);
                next += 1;
            }
            next
        });
        producer.await;
        consumer.await
    });
    assert_eq!(sum, 1000);
}

#[test]
fn many_senders_deliver_every_value() {
    let rt = Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .expect("the runtime starts");
    let (total, count) = rt.block_on(async {
        let (tx, mut rx) = mpsc::channel(3);
        for id in 0..10u64 {
            let tx = tx.clone();
            task::spawn_local(async move {
                for n in 0..20 {
                    tx.send(id * 100 + n).await.unwrap();
                }
            })
            .detach();
        }
        drop(tx);
        let (mut total, mut count) = (0, 0);
        while let Some(n) = rx.recv().await {
            total += n;
            count += 1;
        }
        (total, count)
    });
    let expected: u64 = (0..10u64)
        .map(|id| (0..20).map(|n| id * 100 + n).sum::<u64>())
        .sum();
    assert_eq!((total, count), (expected, 200));
}
