//! Reserved capacity: `reserve`, `reserve_many`, `reserve_owned` and their
//! `try_` forms, the permits they return, and what each error path leaves
//! behind.

use super::*;

const MAX_PERMITS: usize = nuclei::sync::Semaphore::MAX_PERMITS;

#[test]
fn try_reserve_fails_while_the_slot_is_taken_and_sends_or_drops_free_it() {
    let (tx, mut rx) = mpsc::channel(1);

    let permit = tx.try_reserve().unwrap();

    match tx.try_reserve().unwrap_err() {
        TrySendError::Full(()) => {}
        other => panic!("expected Full, got {other:?}"),
    }

    permit.send("foo");
    assert_eq!(block_on(rx.recv()), Some("foo"));

    // Dropping a permit releases the slot.
    let permit = tx.try_reserve().unwrap();
    drop(permit);

    let _permit = tx.try_reserve().unwrap();
}

#[test]
fn reserve_many_above_the_buffer_is_an_error() {
    let (tx, _rx) = mpsc::channel::<()>(1);

    assert!(block_on(tx.reserve_many(2)).is_err());
    assert!(block_on(tx.reserve_many(MAX_PERMITS + 1)).is_err());
    assert!(block_on(tx.reserve_many(usize::MAX)).is_err());
}

#[test]
fn try_reserve_many_of_zero_succeeds_unless_the_channel_is_closed() {
    let (tx, rx) = mpsc::channel::<()>(1);

    // Succeeds when not closed.
    assert!(tx.try_reserve_many(0).unwrap().next().is_none());

    // Even when the channel is full.
    tx.try_send(()).unwrap();
    assert!(tx.try_reserve_many(0).unwrap().next().is_none());

    drop(rx);

    // A closed error when closed.
    assert_eq!(
        tx.try_reserve_many(0).unwrap_err(),
        TrySendError::Closed(())
    );
}

#[test]
fn reserve_many_of_zero_succeeds_unless_the_channel_is_closed() {
    let (tx, rx) = mpsc::channel::<()>(1);

    assert!(block_on(tx.reserve_many(0)).unwrap().next().is_none());

    block_on(tx.send(())).unwrap();
    assert!(block_on(tx.reserve_many(0)).unwrap().next().is_none());

    drop(rx);

    assert!(block_on(tx.reserve_many(0)).is_err());
}

#[test]
fn try_reserve_many_edge_cases() {
    let (tx, rx) = mpsc::channel::<()>(1);

    let mut permit = tx.try_reserve_many(0).unwrap();
    assert!(permit.next().is_none());

    for too_many in [MAX_PERMITS + 1, usize::MAX] {
        match tx.try_reserve_many(too_many).unwrap_err() {
            TrySendError::Full(()) => {}
            other => panic!("expected Full, got {other:?}"),
        }
    }

    // Dropping the receiver closes the channel.
    drop(rx);
    assert!(block_on(tx.reserve_many(0)).is_err());
}

#[test]
fn reserve_many_then_send_each_permit() {
    let (tx, mut rx) = mpsc::channel(100);
    for i in 0..100 {
        for permit in block_on(tx.reserve_many(i)).unwrap() {
            permit.send("foo");
            assert_eq!(block_on(rx.recv()), Some("foo"));
        }
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }
}

#[test]
fn try_reserve_many_then_send_each_permit() {
    let (tx, mut rx) = mpsc::channel(100);
    for i in 0..100 {
        for permit in tx.try_reserve_many(i).unwrap() {
            permit.send("foo");
            assert_eq!(block_on(rx.recv()), Some("foo"));
        }
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }
}

#[test]
fn reserving_on_a_closed_channel_fails() {
    let (tx, rx) = mpsc::channel::<()>(100);
    drop(rx);
    assert!(block_on(tx.reserve()).is_err());
    assert!(block_on(tx.reserve_many(10)).is_err());
    assert!(block_on(tx.clone().reserve_owned()).is_err());

    assert_eq!(tx.try_reserve().unwrap_err(), TrySendError::Closed(()));
    assert_eq!(
        tx.try_reserve_many(10).unwrap_err(),
        TrySendError::Closed(())
    );
    match tx.clone().try_reserve_owned().unwrap_err() {
        TrySendError::Closed(sender) => assert!(sender.same_channel(&tx)),
        other => panic!("expected Closed, got {other:?}"),
    }
}

#[test]
fn try_reserve_many_takes_exactly_what_is_free() {
    // Reserve n slots and send k of them.
    for n in 1..40 {
        for k in 0..n {
            let (tx, mut rx) = mpsc::channel::<usize>(n);
            let permits = tx.try_reserve_many(n).unwrap();

            assert_eq!(permits.len(), n);
            assert_eq!(tx.capacity(), 0);

            match tx.try_reserve_many(1).unwrap_err() {
                TrySendError::Full(..) => {}
                other => panic!("expected Full, got {other:?}"),
            }

            for permit in permits.take(k) {
                permit.send(0);
            }
            // Only k of the n reserved slots were used; the iterator, dropped
            // by `take`, gave the rest back.
            assert_eq!(tx.capacity(), n - k);

            // We can reserve more...
            drop(tx.try_reserve_many(1).unwrap());
            // ...but not more than the current capacity.
            match tx.try_reserve_many(n - k + 1).unwrap_err() {
                TrySendError::Full(..) => {}
                other => panic!("expected Full, got {other:?}"),
            }

            for _ in 0..k {
                assert_eq!(block_on(rx.recv()), Some(0));
            }

            // Everything received: the capacity is back to n.
            assert_eq!(tx.capacity(), n);
        }
    }
}

#[test]
fn dropping_a_permit_unused_frees_its_slot_and_wakes_a_waiting_reserve() {
    let (tx1, _rx) = mpsc::channel::<i32>(1);
    let tx2 = tx1.clone();

    let permit = block_on(tx1.reserve()).unwrap();

    let mut reserve2 = Task::new(tx2.reserve());
    reserve2.pending();

    drop(permit);

    assert!(reserve2.is_woken());
    drop(reserve2.ready().unwrap());
    assert_eq!(tx1.capacity(), 1);
}

#[test]
fn dropping_a_permit_iterator_unused_frees_its_slots() {
    for n in 1..40 {
        let (tx1, _rx) = mpsc::channel::<i32>(n);
        let tx2 = tx1.clone();

        let permits = block_on(tx1.reserve_many(n)).unwrap();

        let mut reserve2 = Task::new(tx2.reserve_many(n));
        reserve2.pending();

        drop(permits);

        assert!(reserve2.is_woken());

        let permits = reserve2.ready().unwrap();
        drop(permits);

        assert_eq!(tx1.capacity(), n);
    }
}

#[test]
fn a_reserve_many_holds_no_slot_while_it_waits_for_all_of_them() {
    // The established API holds the free slots while it waits for the rest;
    // this channel takes all or none, so a smaller request may go ahead.
    let (tx, mut rx) = mpsc::channel(2);
    let held = tx.try_reserve().unwrap();

    let mut many = Task::new(tx.reserve_many(2));
    many.pending();
    assert_eq!(tx.capacity(), 1, "the waiting request holds nothing");
    tx.try_send(1).unwrap();

    drop(held);
    assert_eq!(rx.try_recv(), Ok(1));
    assert!(many.is_woken());
    let permits = many.ready().unwrap();
    assert_eq!(permits.len(), 2);
}

#[test]
fn the_permit_iterator_is_exact_sized_and_fused() {
    let (tx, _rx) = mpsc::channel::<()>(4);
    let mut permits = tx.try_reserve_many(3).unwrap();
    assert_eq!(permits.size_hint(), (3, Some(3)));
    assert_eq!(permits.len(), 3);
    assert!(permits.next().is_some());
    assert_eq!(permits.len(), 2);
    assert!(permits.next().is_some());
    assert!(permits.next().is_some());
    assert!(permits.next().is_none());
    assert!(permits.next().is_none(), "fused");
    assert_eq!(permits.len(), 0);
}

#[test]
fn a_permit_sent_after_the_iterator_was_dropped_still_arrives() {
    let (tx, mut rx) = mpsc::channel::<u8>(3);
    let mut permits = tx.try_reserve_many(3).unwrap();
    let first = permits.next().unwrap();
    drop(permits);
    assert_eq!(tx.capacity(), 2, "the two unissued slots came back");
    first.send(9);
    assert_eq!(rx.try_recv(), Ok(9));
    assert_eq!(tx.capacity(), 3);
}

#[test]
fn reserve_owned_returns_the_sender_through_send_and_release() {
    let (tx, mut rx) = mpsc::channel(1);

    let permit = block_on(tx.reserve_owned()).unwrap();
    let tx = permit.send(123);
    assert_eq!(block_on(rx.recv()), Some(123));
    block_on(tx.send(456)).unwrap();
    assert_eq!(block_on(rx.recv()), Some(456));

    // Reserve through a clone, so the original can show the slot is held.
    let permit = block_on(tx.clone().reserve_owned()).unwrap();
    assert!(tx.try_send(123).is_err());
    let tx2 = permit.release();
    assert!(tx.same_channel(&tx2));
    tx.try_send(789).unwrap();
}

#[test]
fn an_owned_permit_counts_as_a_sender_while_it_lives() {
    let (tx, rx) = mpsc::channel::<()>(2);
    let weak = tx.downgrade();
    assert_eq!(tx.strong_count(), 1);

    let permit = tx.try_reserve_owned().unwrap();
    assert_eq!(rx.sender_strong_count(), 1);
    assert!(
        weak.upgrade().is_some(),
        "upgrade works through the permit's sender"
    );

    drop(permit);
    assert_eq!(rx.sender_strong_count(), 0);
    assert!(weak.upgrade().is_none());
    assert!(rx.is_closed());
}

#[test]
fn dropping_an_owned_permit_frees_the_slot() {
    let (tx, rx) = mpsc::channel::<()>(1);
    let permit = tx.clone().try_reserve_owned().unwrap();
    assert_eq!(tx.capacity(), 0);
    drop(permit);
    assert_eq!(tx.capacity(), 1);
    assert_eq!(tx.strong_count(), 1);
    drop(rx);
}

#[test]
fn try_reserve_owned_on_a_full_channel_hands_the_sender_back() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(1).unwrap();

    let tx = match tx.try_reserve_owned().unwrap_err() {
        TrySendError::Full(sender) => sender,
        other => panic!("expected Full, got {other:?}"),
    };
    assert_eq!(rx.try_recv(), Ok(1));
    tx.try_send(2).unwrap();
}

#[test]
fn owned_permits_know_their_channel() {
    let (tx, _rx) = mpsc::channel::<()>(2);
    let permit1 = block_on(tx.clone().reserve_owned()).unwrap();
    let permit2 = block_on(tx.clone().reserve_owned()).unwrap();
    assert!(permit1.same_channel(&permit2));
    assert!(permit1.same_channel_as_sender(&tx));

    let (tx2, _rx2) = mpsc::channel::<()>(1);
    let permit3 = block_on(tx2.clone().reserve_owned()).unwrap();
    assert!(!permit3.same_channel(&permit2));
    assert!(!permit3.same_channel_as_sender(&tx));
}

#[test]
fn senders_know_their_channel() {
    let (tx, _rx) = mpsc::channel::<()>(1);
    let tx2 = tx.clone();
    assert!(tx.same_channel(&tx2));

    let (tx3, _rx3) = mpsc::channel::<()>(1);
    assert!(!tx3.same_channel(&tx2));
}

#[test]
fn a_waiting_owned_reserve_is_cancel_safe_and_drops_its_sender() {
    let (tx, rx) = mpsc::channel::<()>(1);
    let held = tx.try_reserve().unwrap();

    let mut waiting = Task::new(tx.clone().reserve_owned());
    waiting.pending();
    assert_eq!(tx.strong_count(), 2);
    drop(waiting);
    assert_eq!(tx.strong_count(), 1, "the future's sender went with it");
    assert_eq!(tx.capacity(), 0, "and it holds no slot");

    drop(held);
    assert_eq!(tx.capacity(), 1);
    drop(rx);
}
