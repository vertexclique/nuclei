//! Tests of `nuclei::sync::broadcast`: the behaviour of the established
//! async broadcast-channel API it follows, call by call (order, lag, close,
//! subscription, counts, errors); the deliberate difference (a value is
//! dropped when its slot is taken, not once every receiver took it); wakes;
//! and threads racing sends, receives and drops.
#![cfg(not(target_arch = "wasm32"))]

mod sync_support;

use std::collections::HashMap;
use std::error::Error;
use std::task::Poll;

use nuclei::sync::broadcast::{self, RecvError, SendError, TryRecvError};
use sync_support::{Drops, Task, block_on, eventually};

#[test]
fn every_receiver_gets_every_value_in_order() {
    let (tx, mut a) = broadcast::channel(8);
    let mut b = tx.subscribe();
    for i in 0..5 {
        assert_eq!(tx.send(i), Ok(2));
    }
    for rx in [&mut a, &mut b] {
        for i in 0..5 {
            assert_eq!(rx.try_recv(), Ok(i));
        }
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }
}

#[test]
fn a_send_with_no_receiver_hands_the_value_back() {
    let (tx, rx) = broadcast::channel(2);
    drop(rx);
    assert_eq!(tx.receiver_count(), 0);
    assert_eq!(tx.send("x"), Err(SendError("x")));
    // A subscription opens the channel again.
    let mut rx = tx.subscribe();
    assert_eq!(tx.send("y"), Ok(1));
    assert_eq!(rx.try_recv(), Ok("y"));
}

#[test]
fn a_receiver_takes_what_is_sent_after_it_is_made() {
    let (tx, mut first) = broadcast::channel(4);
    tx.send(1).unwrap();
    let mut late = tx.subscribe();
    let mut later = first.resubscribe();
    tx.send(2).unwrap();
    assert_eq!(first.try_recv(), Ok(1));
    assert_eq!(first.try_recv(), Ok(2));
    assert_eq!(late.try_recv(), Ok(2));
    assert_eq!(later.try_recv(), Ok(2));
    assert_eq!(tx.receiver_count(), 3);
}

/// A receiver that falls behind by more than the capacity hears how many
/// values it lost, once, and goes on from the oldest one still held.
#[test]
fn a_receiver_too_far_behind_hears_how_many_it_lost() {
    // Rounded up to four.
    let (tx, mut rx) = broadcast::channel(3);
    for i in 0..10 {
        tx.send(i).unwrap();
    }
    assert_eq!(rx.len(), 10);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Lagged(6)));
    for i in 6..10 {
        assert_eq!(rx.try_recv(), Ok(i));
    }
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    // The same through `recv`.
    for i in 10..15 {
        tx.send(i).unwrap();
    }
    block_on(async {
        assert_eq!(rx.recv().await, Err(RecvError::Lagged(1)));
        for i in 11..15 {
            assert_eq!(rx.recv().await, Ok(i));
        }
    });
}

#[test]
fn the_channel_closes_after_the_values_left() {
    let (tx, mut rx) = broadcast::channel(4);
    let tx2 = tx.clone();
    tx.send(1).unwrap();
    drop(tx);
    tx2.send(2).unwrap();
    assert_eq!(rx.try_recv(), Ok(1));
    drop(tx2);
    assert_eq!(rx.try_recv(), Ok(2));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(block_on(rx.recv()), Err(RecvError::Closed));
    // A receiver made after the close has nothing, and is closed too.
    let mut again = rx.resubscribe();
    assert_eq!(again.try_recv(), Err(TryRecvError::Closed));
}

#[test]
fn a_waiting_receive_is_woken_by_a_send_and_by_the_close() {
    let (tx, mut rx) = broadcast::channel::<u8>(2);
    let mut recv = Task::new(async move {
        let first = rx.recv().await;
        let second = rx.recv().await;
        (first, second)
    });
    recv.pending();
    tx.send(7).unwrap();
    assert!(recv.is_woken());
    recv.pending();
    drop(tx);
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), (Ok(7), Err(RecvError::Closed)));
}

#[test]
fn closed_completes_once_every_receiver_went() {
    let (tx, rx) = broadcast::channel::<u8>(2);
    let rx2 = tx.subscribe();
    let tx2 = tx.clone();
    let mut closed = Task::new(async move { tx2.closed().await });
    closed.pending();
    drop(rx);
    closed.pending();
    drop(rx2);
    assert!(closed.is_woken());
    assert!(matches!(closed.poll(), Poll::Ready(())));
    drop(tx);
}

#[test]
fn lengths_count_what_is_yet_to_be_taken() {
    let (tx, mut slow) = broadcast::channel(4);
    let mut fast = tx.subscribe();
    assert!(tx.is_empty() && slow.is_empty());
    for i in 0..3 {
        tx.send(i).unwrap();
    }
    for _ in 0..3 {
        fast.try_recv().unwrap();
    }
    assert_eq!((tx.len(), slow.len(), fast.len()), (3, 3, 0));
    slow.try_recv().unwrap();
    assert_eq!(tx.len(), 2);
    for i in 3..20 {
        tx.send(i).unwrap();
    }
    // At most the capacity: the slowest lost the older ones.
    assert_eq!(tx.len(), 4);
    assert_eq!(slow.len(), 19);
    drop((slow, fast));
    assert_eq!(tx.len(), 0);
}

#[test]
fn same_channel_tells_the_senders_apart() {
    let (a, _ra) = broadcast::channel::<u8>(1);
    let (b, _rb) = broadcast::channel::<u8>(1);
    assert!(a.same_channel(&a.clone()));
    assert!(!a.same_channel(&b));
}

#[test]
#[should_panic(expected = "cannot be zero")]
fn a_capacity_of_zero_is_refused() {
    let _ = broadcast::channel::<u8>(0);
}

#[test]
fn errors_display_and_compare_like_the_established_api() {
    assert_eq!(SendError(1).to_string(), "channel closed");
    assert_eq!(RecvError::Closed.to_string(), "channel closed");
    assert_eq!(RecvError::Lagged(3).to_string(), "channel lagged by 3");
    assert_eq!(TryRecvError::Empty.to_string(), "channel empty");
    assert_eq!(TryRecvError::Lagged(2).to_string(), "channel lagged by 2");
    let boxed: Box<dyn Error> = Box::new(TryRecvError::Closed);
    assert_eq!(boxed.to_string(), "channel closed");
}

/// The deliberate difference: a value stays until a later send takes its
/// slot, not until every receiver took it; each value is dropped once.
#[test]
fn a_value_is_dropped_once_when_its_slot_is_taken() {
    let drops = Drops::new();
    let (tx, mut rx) = broadcast::channel(2);
    tx.send(drops.value()).unwrap();
    drop(rx.try_recv().unwrap());
    // The receiver's clone went; the channel's copy stays in its slot.
    assert_eq!(drops.count(), 1);
    tx.send(drops.value()).unwrap();
    tx.send(drops.value()).unwrap();
    // The third send took the first one's slot: that value goes once kovan
    // reclaims its node.
    eventually("the first value to be dropped", || drops.count() >= 2);
    drop((tx, rx));
    eventually("every value to be dropped", || drops.count() == 4);
}

/// Senders and receivers on threads: each receiver sees each sender's
/// values in order, never one twice, and the values it did not see are
/// exactly the ones its lag reports counted.
#[test]
fn threads_lose_nothing_but_what_the_lags_report() {
    const SENDERS: usize = 4;
    const RECEIVERS: usize = 4;
    const PER_SENDER: u64 = 20_000;
    for capacity in [16, 1 << 17] {
        let (tx, rx) = broadcast::channel::<(usize, u64)>(capacity);
        let receivers: Vec<_> = (0..RECEIVERS).map(|_| rx.resubscribe()).collect();
        drop(rx);
        let readers: Vec<_> = receivers
            .into_iter()
            .map(|mut rx| {
                std::thread::spawn(move || {
                    let mut last: HashMap<usize, u64> = HashMap::new();
                    let (mut got, mut lost) = (0u64, 0u64);
                    block_on(async {
                        loop {
                            match rx.recv().await {
                                Ok((sender, n)) => {
                                    if let Some(&before) = last.get(&sender) {
                                        assert!(n > before, "sender {sender}: {n} after {before}");
                                    }
                                    last.insert(sender, n);
                                    got += 1;
                                }
                                Err(RecvError::Lagged(missed)) => lost += missed,
                                Err(RecvError::Closed) => break,
                            }
                        }
                    });
                    (got, lost)
                })
            })
            .collect();
        let writers: Vec<_> = (0..SENDERS)
            .map(|sender| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for n in 0..PER_SENDER {
                        tx.send((sender, n)).unwrap();
                    }
                })
            })
            .collect();
        drop(tx);
        for writer in writers {
            writer.join().unwrap();
        }
        for reader in readers {
            let (got, lost) = reader.join().unwrap();
            assert_eq!(
                got + lost,
                SENDERS as u64 * PER_SENDER,
                "capacity {capacity}: {got} taken, {lost} reported lost"
            );
            if capacity == 1 << 17 {
                assert_eq!(lost, 0, "no lag with room for every value");
            }
        }
    }
}
