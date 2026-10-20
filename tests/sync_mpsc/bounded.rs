//! The bounded channel: sending and receiving through a buffer, backpressure,
//! the errors of a full or closed channel, capacity, length and the closed
//! state as the receiver sees it, and the polls.

use super::*;

#[test]
fn values_come_out_in_the_order_they_went_in() {
    let (tx, mut rx) = mpsc::channel::<i32>(16);

    // A reserved permit, then a direct try_send.
    let permit = block_on(tx.reserve()).unwrap();
    permit.send(1);
    tx.try_send(2).unwrap();
    drop(tx);

    assert_eq!(block_on(rx.recv()), Some(1));
    assert_eq!(block_on(rx.recv()), Some(2));
    assert_eq!(block_on(rx.recv()), None);
}

#[test]
fn a_full_channel_holds_back_reservations_until_slots_come_back() {
    let (tx, mut rx) = mpsc::channel::<i32>(2);
    let (tx1, tx2, tx3, tx4) = (tx.clone(), tx.clone(), tx.clone(), tx);

    let permit1 = block_on(tx1.reserve()).unwrap();
    let permit2 = block_on(tx2.reserve()).unwrap();

    // Two slots are taken, so a third and a fourth reservation wait.
    let mut r3 = Task::new(tx3.reserve());
    r3.pending();
    let mut r4 = Task::new(tx4.reserve());
    r4.pending();

    // Using a reserved slot does not free it: the receiver must take the value.
    permit1.send(1);
    assert!(!r3.is_woken());
    block_on(rx.recv()).unwrap();
    assert!(
        r3.is_woken(),
        "a received value frees a slot for the oldest"
    );
    assert!(!r4.is_woken(), "one slot is for one waiter");

    // Dropping a permit unused frees a slot too. The semaphore has one nudge
    // on its way already (r3's, not yet polled), so this release leaves the
    // next wake to the poll that consumes it.
    drop(permit2);

    // This channel lets a caller take what is free even while others wait
    // (see the semaphore), so both waiters find a slot when polled; and the
    // slot r3 leaves free wakes r4 in turn.
    let permit3 = r3.ready().unwrap();
    assert!(r4.is_woken(), "a free slot with a waiter queued wakes it");
    let permit4 = r4.ready().unwrap();
    drop((permit3, permit4));
    assert_eq!(tx1.capacity(), 2);
}

#[test]
fn a_reservation_that_gives_up_its_place_passes_the_wake_on() {
    let (tx1, mut rx) = mpsc::channel::<()>(1);
    let tx2 = tx1.clone();
    let mut first = Probe::new();

    tx1.try_send(()).unwrap();

    let mut r1 = Box::pin(tx1.reserve());
    first.enter(|cx| assert!(r1.as_mut().poll(cx).is_pending()));

    {
        let mut r2 = Task::new(tx2.reserve());
        r2.pending();

        drop(r1);
        assert!(block_on(rx.recv()).is_some());

        assert!(r2.is_woken(), "the next waiter is woken");
        assert!(!first.is_woken(), "the one that left is not");
    }

    drop(tx1);
    drop(tx2);
    assert!(block_on(rx.recv()).is_none());
}

#[test]
#[should_panic(expected = "buffer > 0")]
fn a_buffer_of_zero_panics() {
    let _ = mpsc::channel::<i32>(0);
}

#[test]
#[should_panic(expected = "MAX_PERMITS")]
fn a_buffer_beyond_the_semaphore_limit_panics() {
    let _ = mpsc::channel::<i32>(nuclei::sync::Semaphore::MAX_PERMITS + 1);
}

#[test]
fn a_send_waits_for_the_receiver_and_is_woken_by_it() {
    let (tx, mut rx) = mpsc::channel::<i32>(1);

    let p1 = block_on(tx.reserve()).unwrap();
    p1.send(1);

    let mut p2 = Task::new(tx.reserve());
    p2.pending();

    assert!(block_on(rx.recv()).is_some());
    assert!(p2.is_woken());

    // The slot the receiver freed is not taken yet, so a direct send finds it.
    // Reserve first, then prove the channel is full.
    let permit = p2.ready().unwrap();
    assert!(tx.try_send(1337).is_err());
    permit.send(2);
    assert!(block_on(rx.recv()).is_some());
}

#[test]
fn try_send_says_full_and_hands_the_value_back() {
    let (tx, mut rx) = mpsc::channel(1);

    tx.try_send("hello").unwrap();
    match tx.try_send("fail").unwrap_err() {
        TrySendError::Full(value) => assert_eq!(value, "fail"),
        other => panic!("expected Full, got {other:?}"),
    }

    assert_eq!(block_on(rx.recv()), Some("hello"));

    tx.try_send("goodbye").unwrap();
    drop(tx);

    assert_eq!(block_on(rx.recv()), Some("goodbye"));
    assert_eq!(block_on(rx.recv()), None);
}

#[test]
fn try_send_and_try_recv_share_one_buffer() {
    let (tx, mut rx) = mpsc::channel(1);

    tx.try_send("hello").unwrap();
    assert!(matches!(tx.try_send("fail"), Err(TrySendError::Full(_))));
    assert_eq!(rx.try_recv(), Ok("hello"));

    tx.try_send("goodbye").unwrap();
    drop(tx);

    assert_eq!(rx.try_recv(), Ok("goodbye"));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn try_recv_walks_a_buffer_through_full_and_empty() {
    let (tx, mut rx) = mpsc::channel(5);

    for _ in 0..5 {
        tx.try_send("hello").unwrap();
    }
    assert!(tx.try_send("hello").is_err());
    for _ in 0..5 {
        assert_eq!(rx.try_recv(), Ok("hello"));
    }
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

    for _ in 0..4 {
        tx.try_send("hello").unwrap();
    }
    assert_eq!(rx.try_recv(), Ok("hello"));
    tx.try_send("hello").unwrap();
    tx.try_send("hello").unwrap();
    assert!(tx.try_send("hello").is_err());
    for _ in 0..5 {
        assert_eq!(rx.try_recv(), Ok("hello"));
    }
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));

    for _ in 0..3 {
        tx.try_send("hello").unwrap();
    }
    drop(tx);
    for _ in 0..3 {
        assert_eq!(rx.try_recv(), Ok("hello"));
    }
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn try_recv_reports_empty_while_a_sender_lives_and_disconnected_after() {
    let (tx, mut rx) = mpsc::channel::<()>(5);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    drop(tx);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn a_send_to_a_dropped_receiver_returns_its_value() {
    let (tx, rx) = mpsc::channel(4);
    drop(rx);

    let SendError(value) = block_on(tx.send(String::from("lost"))).unwrap_err();
    assert_eq!(value, "lost");

    match tx.try_send(String::from("lost too")).unwrap_err() {
        TrySendError::Closed(value) => assert_eq!(value, "lost too"),
        other => panic!("expected Closed, got {other:?}"),
    }
    assert!(tx.is_closed());
}

#[test]
fn a_send_waiting_for_room_fails_with_its_value_when_the_receiver_drops() {
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(String::from("holds the slot")).unwrap();

    let mut waiting = Task::new(tx.send(String::from("waiting")));
    waiting.pending();
    drop(rx);

    assert!(waiting.is_woken());
    let SendError(value) = waiting.ready().unwrap_err();
    assert_eq!(value, "waiting");
}

#[test]
fn a_send_future_dropped_early_sends_nothing_and_drops_its_value() {
    let drops = Drops::new();
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(drops.value()).unwrap();

    let mut waiting = Task::new(tx.send(drops.value()));
    waiting.pending();
    drop(waiting);
    assert_eq!(
        drops.count(),
        1,
        "the unsent value was dropped with the future"
    );

    drop(rx.try_recv().unwrap());
    assert_eq!(drops.count(), 2);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(tx.capacity(), 1, "the dropped send holds no slot");
}

#[test]
fn a_recv_future_dropped_early_takes_nothing() {
    let (tx, mut rx) = mpsc::channel(4);

    let mut waiting = Task::new(rx.recv());
    waiting.pending();
    drop(waiting);

    tx.try_send(7).unwrap();
    assert_eq!(block_on(rx.recv()), Some(7));
}

#[test]
fn a_send_from_another_thread_wakes_a_waiting_recv() {
    let (tx, mut rx) = mpsc::channel(16);

    let sender = std::thread::spawn(move || {
        block_on(tx.send(1)).unwrap();
        block_on(tx.send(2)).unwrap();
    });

    assert_eq!(block_on(rx.recv()), Some(1));
    assert_eq!(block_on(rx.recv()), Some(2));
    assert_eq!(block_on(rx.recv()), None);
    sender.join().unwrap();
}

#[test]
fn the_buffer_applies_backpressure_across_threads() {
    let (tx, mut rx) = mpsc::channel(1);
    let sender = std::thread::spawn(move || {
        for i in 0..50 {
            block_on(tx.send(i)).unwrap();
        }
    });
    for i in 0..50 {
        assert_eq!(block_on(rx.recv()), Some(i));
    }
    assert_eq!(block_on(rx.recv()), None);
    sender.join().unwrap();
}

#[test]
fn the_channel_needs_no_bounds_on_the_message_type() {
    struct NoImpls;

    let (tx, mut rx) = mpsc::channel(100);

    // The sender is Debug even though T is not, and the receiver too.
    is_debug(&tx);
    is_debug(&rx);
    // And the sender is Clone even though T is not.
    assert!(tx.clone().try_send(NoImpls).is_ok());

    assert!(block_on(rx.recv()).is_some());
}

#[test]
fn debug_output_names_the_handles_and_shows_no_payload() {
    let (tx, rx) = mpsc::channel(3);
    tx.try_send(String::from("secret")).unwrap();
    let sender = format!("{tx:?}");
    let receiver = format!("{rx:?}");
    let weak = format!("{:?}", tx.downgrade());
    assert!(sender.starts_with("Sender"), "{sender}");
    assert!(receiver.starts_with("Receiver"), "{receiver}");
    assert!(weak.starts_with("WeakSender"), "{weak}");
    for text in [sender, receiver, weak] {
        assert!(!text.contains("secret"), "{text}");
    }
}

#[test]
fn capacity_falls_with_reservations_and_sends_and_rises_with_receives() {
    let (tx, mut rx) = mpsc::channel::<()>(10);
    assert_eq!((tx.capacity(), tx.max_capacity()), (10, 10));
    assert_eq!((rx.capacity(), rx.max_capacity()), (10, 10));

    let _permit = block_on(tx.reserve()).unwrap();
    assert_eq!((tx.capacity(), tx.max_capacity()), (9, 10));

    block_on(tx.send(())).unwrap();
    assert_eq!((tx.capacity(), tx.max_capacity()), (8, 10));
    assert_eq!(rx.len(), 1);

    block_on(rx.recv()).unwrap();
    assert_eq!((rx.capacity(), rx.max_capacity()), (9, 10));
    assert_eq!(rx.len(), 0);
}

#[test]
fn the_receiver_is_closed_after_close_and_after_the_last_sender_drops() {
    // close() with a sender alive.
    let (_tx, mut rx) = mpsc::channel::<()>(10);
    assert!(!rx.is_closed());
    rx.close();
    assert!(rx.is_closed());

    // Every sender dropped, one of them on another thread.
    let (tx, rx) = mpsc::channel::<()>(10);
    let other = tx.clone();
    let thread = std::thread::spawn(move || drop(other));
    drop(tx);
    thread.join().unwrap();
    assert!(rx.is_closed());

    // A live sender keeps it open, even with the buffer full.
    let (tx, rx) = mpsc::channel(10);
    for i in 0..10 {
        block_on(tx.send(i)).unwrap();
    }
    assert!(!rx.is_closed());
    // But with no sender left it is closed, buffered values or not.
    drop(tx);
    assert!(rx.is_closed());

    // Closing keeps what is buffered.
    let (tx, mut rx) = mpsc::channel(10);
    for i in 0..10 {
        block_on(tx.send(i)).unwrap();
    }
    rx.close();
    assert!(rx.is_closed());
    assert_eq!(rx.len(), 10);
}

#[test]
fn an_owned_permit_keeps_the_channel_open_without_a_sender() {
    let (tx, rx) = mpsc::channel::<()>(10);
    let _permit = block_on(tx.reserve_owned()).expect("reserve");
    assert!(!rx.is_closed());
}

#[test]
fn is_empty_and_len_follow_the_buffer() {
    let (tx, mut rx) = mpsc::channel(10);
    assert!(rx.is_empty());
    assert_eq!(rx.len(), 0);

    for i in 0..10 {
        block_on(tx.send(i)).unwrap();
        assert_eq!(rx.len(), i + 1);
        assert!(!rx.is_empty());
    }
    drop(tx);
    assert_eq!(
        rx.len(),
        10,
        "dropping the senders does not change the length"
    );

    for i in (0..10).rev() {
        assert!(block_on(rx.recv()).is_some());
        assert_eq!(rx.len(), i);
    }
    assert!(rx.is_empty());
}

#[test]
fn len_survives_close_and_the_last_sender_in_either_order() {
    for sender_first in [false, true] {
        let (tx, mut rx) = mpsc::channel(100);
        block_on(tx.send(())).unwrap();
        if sender_first {
            drop(tx);
            rx.close();
        } else {
            rx.close();
            drop(tx);
        }
        assert_eq!(rx.len(), 1);
    }

    let (tx, rx) = mpsc::channel::<()>(100);
    drop(tx);
    assert_eq!(rx.len(), 0, "the end of the channel is not a value");
    assert!(rx.is_empty());
}

/// The segment queue behind the channel moves on to a new segment every few
/// values; an empty channel stays empty across many of them.
#[test]
fn is_empty_holds_after_every_receive_across_segments() {
    let (sender, mut receiver) = mpsc::channel(33);

    for value in 1..257 {
        block_on(sender.send(value)).unwrap();
        block_on(receiver.recv()).unwrap();
        assert!(receiver.is_empty(), "{value}. len: {}", receiver.len());
    }
}

#[test]
fn poll_recv_keeps_only_the_latest_waker() {
    let (tx, mut rx) = mpsc::channel(4);
    let (first, first_waker) = counting_waker();
    let (second, second_waker) = counting_waker();

    assert_eq!(
        rx.poll_recv(&mut Context::from_waker(&first_waker)),
        Poll::Pending
    );
    assert_eq!(
        rx.poll_recv(&mut Context::from_waker(&second_waker)),
        Poll::Pending
    );

    tx.try_send(1).unwrap();
    assert_eq!(wakes(&first), 0, "a replaced waker is not woken");
    assert_eq!(wakes(&second), 1);
    assert_eq!(rx.poll_recv(&mut noop_context()), Poll::Ready(Some(1)));
    assert_eq!(rx.poll_recv(&mut noop_context()), Poll::Pending);

    drop(tx);
    assert_eq!(rx.poll_recv(&mut noop_context()), Poll::Ready(None));
}

#[test]
fn the_last_sender_dropping_wakes_a_pending_receiver() {
    let (tx, mut rx) = mpsc::channel::<u8>(4);
    let mut recv = Task::new(rx.recv());
    recv.pending();
    drop(tx);
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), None);
}

#[test]
fn blocking_send_and_blocking_recv_cross_threads() {
    let (tx, mut rx) = mpsc::channel::<u8>(1);

    let sync_code = std::thread::spawn(move || {
        assert_eq!(Some(10), rx.blocking_recv());
    });
    block_on(async move {
        let _ = tx.send(10).await;
    });
    sync_code.join().unwrap();

    let (tx, mut rx) = mpsc::channel::<u8>(1);
    let sync_code = std::thread::spawn(move || {
        tx.blocking_send(10).unwrap();
    });
    block_on(async move {
        assert_eq!(Some(10), rx.recv().await);
    });
    sync_code.join().unwrap();
}

#[test]
fn blocking_send_waits_for_room_and_fails_on_a_closed_channel() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.blocking_send(1).unwrap();

    let sender = std::thread::spawn({
        let tx = tx.clone();
        move || tx.blocking_send(2)
    });
    // The thread is parked on the full buffer until this receive.
    assert_eq!(rx.blocking_recv(), Some(1));
    sender.join().unwrap().unwrap();
    assert_eq!(rx.blocking_recv(), Some(2));

    drop(rx);
    let SendError(value) = tx.blocking_send(3).unwrap_err();
    assert_eq!(value, 3);
}

#[test]
fn the_message_may_be_a_trait_object() {
    let (tx, mut rx) = mpsc::channel::<Box<dyn Fn() -> u32 + Send>>(2);
    tx.try_send(Box::new(|| 41 + 1)).unwrap();
    let job = rx.try_recv().unwrap();
    assert_eq!(job(), 42);
}

#[test]
fn the_unwind_safety_of_the_handles_matches_the_established_api() {
    fn assert_unwind_safe<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}
    assert_unwind_safe::<mpsc::Sender<u8>>();
    assert_unwind_safe::<mpsc::WeakSender<u8>>();
    assert_unwind_safe::<mpsc::Receiver<u8>>();
    assert_unwind_safe::<mpsc::UnboundedSender<u8>>();
    assert_unwind_safe::<mpsc::WeakUnboundedSender<u8>>();
    assert_unwind_safe::<mpsc::UnboundedReceiver<u8>>();
    assert_unwind_safe::<mpsc::OwnedPermit<u8>>();
    assert_unwind_safe::<mpsc::Permit<'static, u8>>();
    assert_unwind_safe::<mpsc::PermitIterator<'static, u8>>();

    let (tx, mut rx) = mpsc::channel::<u8>(2);
    let result = catch_unwind(AssertUnwindSafe(|| {
        tx.try_send(1).unwrap();
        panic!("a panic between sends");
    }));
    assert!(result.is_err());
    assert_eq!(
        rx.try_recv(),
        Ok(1),
        "the channel is usable after the panic"
    );
}
