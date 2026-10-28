//! Drop accounting: every value is dropped exactly once on every path (taken
//! by the receiver, queued when the receiver drops, refused by a closed
//! channel, left when the last handle goes), and a panic in a destructor or in
//! a waker leaves the channel consistent.

use super::*;

#[test]
fn unconsumed_messages_are_dropped_with_the_channel() {
    let msg = Arc::new(());

    let (tx, rx) = mpsc::channel(100);

    tx.try_send(Arc::clone(&msg)).unwrap();

    assert_eq!(2, Arc::strong_count(&msg));

    drop((tx, rx));

    assert_eq!(1, Arc::strong_count(&msg));

    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(Arc::clone(&msg)).unwrap();
    assert_eq!(2, Arc::strong_count(&msg));
    drop((tx, rx));
    assert_eq!(1, Arc::strong_count(&msg));
}

#[test]
fn a_received_value_is_dropped_by_its_receiver_and_never_again() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::channel(4);
    tx.try_send(drops.value()).unwrap();
    tx.try_send(drops.value()).unwrap();
    assert_eq!(drops.count(), 0);

    let value = rx.try_recv().unwrap();
    assert_eq!(drops.count(), 0, "a received value is the receiver's now");
    drop(value);
    assert_eq!(drops.count(), 1);

    drop(rx);
    assert_eq!(
        drops.count(),
        2,
        "the one still queued went with the receiver"
    );
    drop(tx);
    assert_eq!(drops.count(), 2, "and nothing is dropped again");
}

#[test]
fn values_queued_when_the_receiver_drops_are_dropped_at_once() {
    for bounded in [true, false] {
        let drops = Drops::new();
        if bounded {
            let (tx, rx) = mpsc::channel(8);
            for _ in 0..5 {
                tx.try_send(drops.value()).unwrap();
            }
            drop(rx);
            assert_eq!(drops.count(), 5, "bounded: dropped by the receiver's drop");
            assert_eq!(tx.capacity(), 8, "and their slots came back");
        } else {
            let (tx, rx) = mpsc::unbounded_channel();
            for _ in 0..5 {
                tx.send(drops.value()).unwrap();
            }
            drop(rx);
            assert_eq!(
                drops.count(),
                5,
                "unbounded: dropped by the receiver's drop"
            );
            drop(tx);
        }
    }
}

#[test]
fn a_value_refused_by_a_closed_channel_comes_back_and_is_dropped_once() {
    let drops = Drops::new();
    let (tx, rx) = mpsc::channel(2);
    drop(rx);

    let SendError(back) = block_on(tx.send(drops.value())).unwrap_err();
    assert_eq!(drops.count(), 0, "returned, not dropped");
    drop(back);
    assert_eq!(drops.count(), 1);

    let refused = tx.try_send(drops.value()).unwrap_err();
    assert!(matches!(refused, TrySendError::Closed(_)));
    assert_eq!(drops.count(), 1);
    drop(refused.into_inner());
    assert_eq!(drops.count(), 2);

    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    drop(tx.send(drops.value()).unwrap_err());
    assert_eq!(drops.count(), 3);
}

#[test]
fn a_full_channel_hands_the_refused_value_back_untouched() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(drops.value()).unwrap();
    let refused = tx.try_send(drops.value()).unwrap_err();
    assert!(matches!(refused, TrySendError::Full(_)));
    assert_eq!(drops.count(), 0);
    drop(refused);
    assert_eq!(drops.count(), 1);
    drop(rx.try_recv().unwrap());
    assert_eq!(drops.count(), 2);
}

#[test]
fn the_queue_of_a_closed_channel_is_dropped_by_the_receiver_even_with_senders_alive() {
    let msgs: Vec<_> = (0..3).map(|_| Arc::new(())).collect();
    let (tx, rx) = mpsc::channel(3);
    let weak = tx.downgrade();
    for msg in &msgs {
        tx.try_send(Arc::clone(msg)).unwrap();
    }
    drop(rx);
    for msg in &msgs {
        assert_eq!(1, Arc::strong_count(msg));
    }
    drop((tx, weak));
}

/// Bounded: three values are sent, one of them by a send that waited for a
/// slot, the receiver drops, and all three are dropped exactly then; the next
/// send is refused and its value returned.
#[test]
fn messages_sent_before_the_receiver_drops_are_dropped_then_and_later_ones_are_refused() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::channel(3);

    block_on(tx.send(drops.value())).unwrap();
    block_on(tx.send(drops.value())).unwrap();
    block_on(tx.send(drops.value())).unwrap();

    // A fourth send waits for a slot.
    let mut pending = Task::new(tx.send(drops.value()));
    pending.pending();

    drop(rx.try_recv().unwrap());
    drop(rx.try_recv().unwrap());
    assert_eq!(drops.count(), 2);

    pending.ready().unwrap();
    drop(rx);
    assert_eq!(drops.count(), 4, "every value that was sent is dropped");

    // The channel is closed, so this value is not queued: it comes back.
    assert!(block_on(tx.send(drops.value())).is_err());
    assert_eq!(drops.count(), 5);
}

#[test]
fn unbounded_messages_sent_before_the_receiver_drops_are_dropped_then() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::unbounded_channel();

    tx.send(drops.value()).unwrap();
    tx.send(drops.value()).unwrap();
    tx.send(drops.value()).unwrap();

    drop(block_on(rx.recv()).unwrap());
    drop(block_on(rx.recv()).unwrap());

    drop(rx);
    assert_eq!(drops.count(), 3);

    assert!(tx.send(drops.value()).is_err());
    assert_eq!(drops.count(), 4);
}

#[test]
fn a_panicking_destructor_does_not_stop_the_receiver_from_dropping_the_rest() {
    struct A(bool, Arc<AtomicUsize>);
    impl Drop for A {
        fn drop(&mut self) {
            self.1.fetch_add(1, Ordering::Relaxed);
            // A panic when the inner flag is set.
            if self.0 {
                panic!("a destructor panicked");
            }
        }
    }

    for bounded in [false, true] {
        let counter = Arc::new(AtomicUsize::new(0));
        let make = |panics| A(panics, Arc::clone(&counter));

        let result = if bounded {
            let (tx, rx) = mpsc::channel(8);
            tx.try_send(make(true)).unwrap();
            tx.try_send(make(false)).unwrap();
            tx.try_send(make(false)).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| drop(rx)));
            // Every slot came back although the first destructor panicked.
            assert_eq!(tx.capacity(), 8);
            result
        } else {
            let (tx, rx) = mpsc::unbounded_channel();
            tx.send(make(true)).unwrap();
            tx.send(make(false)).unwrap();
            tx.send(make(false)).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| drop(rx)));
            drop(tx);
            result
        };

        assert!(result.is_err(), "the destructor's panic is not swallowed");
        // The receiver's drop carried on after the panic: all three are
        // dropped before the channel itself is.
        assert_eq!(counter.load(Ordering::Relaxed), 3, "bounded: {bounded}");
    }
}

#[test]
fn a_panicking_receiver_waker_loses_neither_the_value_nor_its_slot() {
    struct PanickingWaker;

    impl Wake for PanickingWaker {
        fn wake(self: Arc<Self>) {
            panic!("waker panicked");
        }
    }

    let (tx, mut rx) = mpsc::channel(1);
    let waker = Waker::from(Arc::new(PanickingWaker));
    let mut cx = Context::from_waker(&waker);
    assert!(rx.poll_recv(&mut cx).is_pending());

    let permit = tx.try_reserve().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| permit.send(7))).is_err());

    assert_eq!(tx.capacity(), 0);
    assert_eq!(rx.len(), 1);

    assert_eq!(rx.try_recv(), Ok(7));
    assert_eq!(tx.capacity(), 1);
    assert_eq!(tx.max_capacity(), 1);

    rx.close();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));

    // The same through an unbounded send.
    let (tx, mut rx) = mpsc::unbounded_channel();
    assert!(rx.poll_recv(&mut cx).is_pending());
    assert!(catch_unwind(AssertUnwindSafe(|| tx.send(8))).is_err());
    assert_eq!(rx.try_recv(), Ok(8));
}

#[test]
fn a_send_failed_by_the_receiver_drop_returns_its_value_and_the_queue_is_dropped() {
    let drops = Drops::new();
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(drops.value()).unwrap();

    let mut sending = Task::new(tx.send(drops.value()));
    sending.pending();
    drop(rx);
    // The close failed the send, which holds its value in the output.
    assert!(sending.ready().is_err());
    assert_eq!(
        drops.count(),
        2,
        "both the failed send's value and the queued one"
    );
}

#[test]
fn recv_many_hands_every_value_to_the_buffer_exactly_once() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    for _ in 0..10 {
        tx.send(drops.value()).unwrap();
    }
    let mut buffer = Vec::new();
    assert_eq!(10, block_on(rx.recv_many(&mut buffer, 100)));
    assert_eq!(drops.count(), 0);
    drop(rx);
    assert_eq!(drops.count(), 0, "the buffer owns them now");
    buffer.clear();
    assert_eq!(drops.count(), 10);
}

#[test]
fn the_last_handle_to_go_drops_what_a_race_with_the_receiver_left() {
    // A permit reserved before the receiver dropped, used after: the value is
    // in the queue with nobody to receive it.
    let drops = Drops::new();
    let (tx, rx) = mpsc::channel(4);
    let permits: Vec<_> = (0..3).map(|_| tx.try_reserve().unwrap()).collect();
    drop(rx);
    for permit in permits {
        permit.send(drops.value());
    }
    assert_eq!(drops.count(), 0, "still held by the channel");
    let weak = tx.downgrade();
    drop(tx);
    assert_eq!(drops.count(), 0, "a weak sender holds the channel");
    drop(weak);
    assert_eq!(drops.count(), 3, "freed with the last handle, once each");
}
