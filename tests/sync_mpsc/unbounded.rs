//! The unbounded channel: synchronous sends, receiving, the closed state,
//! length, and the polls.

use super::*;

#[test]
fn values_come_out_in_the_order_they_went_in() {
    let (tx, mut rx) = mpsc::unbounded_channel::<i32>();

    tx.send(1).unwrap();
    tx.send(2).unwrap();

    assert_eq!(block_on(rx.recv()), Some(1));
    assert_eq!(block_on(rx.recv()), Some(2));

    drop(tx);
    assert_eq!(block_on(rx.recv()), None);
}

#[test]
fn a_send_never_waits_however_many_values_are_queued() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    for i in 0..10_000 {
        tx.send(i).unwrap();
    }
    assert_eq!(rx.len(), 10_000);
    for i in 0..10_000 {
        assert_eq!(rx.try_recv(), Ok(i));
    }
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn a_send_from_another_thread_wakes_a_waiting_recv() {
    let (tx, mut rx) = mpsc::unbounded_channel();

    let sender = std::thread::spawn(move || {
        tx.send(1).unwrap();
        tx.send(2).unwrap();
    });

    assert_eq!(block_on(rx.recv()), Some(1));
    assert_eq!(block_on(rx.recv()), Some(2));
    assert_eq!(block_on(rx.recv()), None);
    sender.join().unwrap();
}

#[test]
fn a_send_to_a_closed_or_dropped_receiver_returns_its_value() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    rx.close();
    let SendError(value) = tx.send(String::from("closed")).unwrap_err();
    assert_eq!(value, "closed");
    assert!(tx.is_closed());

    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let SendError(value) = tx.send(String::from("dropped")).unwrap_err();
    assert_eq!(value, "dropped");
    assert_eq!(
        tx.send(String::new()).unwrap_err().to_string(),
        "channel closed"
    );
}

#[test]
fn the_channel_needs_no_bounds_on_the_message_type() {
    struct NoImpls;

    let (tx, mut rx) = mpsc::unbounded_channel();

    is_debug(&tx);
    is_debug(&rx);
    assert!(tx.clone().send(NoImpls).is_ok());

    assert!(block_on(rx.recv()).is_some());
}

#[test]
fn try_recv_walks_through_values_empty_and_disconnected() {
    for num in 0..100 {
        let (tx, mut rx) = mpsc::unbounded_channel();

        for i in 0..num {
            tx.send(i).unwrap();
        }
        for i in 0..num {
            assert_eq!(rx.try_recv(), Ok(i));
        }

        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(tx);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }
}

#[test]
fn try_recv_reports_empty_while_a_sender_lives_and_disconnected_after() {
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    drop(tx);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn the_receiver_is_closed_after_close_and_after_the_last_sender_drops() {
    let (_tx, mut rx) = mpsc::unbounded_channel::<()>();
    assert!(!rx.is_closed());
    rx.close();
    assert!(rx.is_closed());

    let (tx, rx) = mpsc::unbounded_channel::<()>();
    let other = tx.clone();
    let thread = std::thread::spawn(move || drop(other));
    drop(tx);
    thread.join().unwrap();
    assert!(rx.is_closed());

    let (_tx, rx) = mpsc::unbounded_channel::<()>();
    assert!(!rx.is_closed());

    let (tx, rx) = mpsc::unbounded_channel();
    for i in 0..10 {
        tx.send(i).unwrap();
    }
    drop(tx);
    assert!(rx.is_closed(), "closed with buffered values and no sender");

    let (tx, mut rx) = mpsc::unbounded_channel();
    for i in 0..10 {
        tx.send(i).unwrap();
    }
    rx.close();
    assert!(rx.is_closed(), "closed with buffered values and a sender");
}

#[test]
fn is_empty_and_len_follow_the_queue() {
    let (_tx, rx) = mpsc::unbounded_channel::<()>();
    assert!(rx.is_empty());
    assert_eq!(rx.len(), 0);

    let (tx, mut rx) = mpsc::unbounded_channel();
    for i in 0..100 {
        tx.send(i).unwrap();
        assert_eq!(rx.len(), i + 1);
        assert!(!rx.is_empty());
    }
    drop(tx);
    assert_eq!(rx.len(), 100);

    for i in (0..100).rev() {
        assert!(block_on(rx.recv()).is_some());
        assert_eq!(rx.len(), i);
    }
    assert!(rx.is_empty());

    let (tx, mut rx) = mpsc::unbounded_channel();
    for i in 0..10 {
        tx.send(i).unwrap();
    }
    while rx.try_recv().is_ok() {}
    assert!(rx.is_empty());
}

#[test]
fn len_survives_close_and_the_last_sender_in_either_order() {
    for sender_first in [false, true] {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(()).unwrap();
        if sender_first {
            drop(tx);
            rx.close();
        } else {
            rx.close();
            drop(tx);
        }
        assert_eq!(rx.len(), 1);
    }

    let (tx, rx) = mpsc::unbounded_channel::<()>();
    drop(tx);
    assert_eq!(rx.len(), 0);
}

#[test]
fn poll_recv_keeps_only_the_latest_waker() {
    let (tx, mut rx) = mpsc::unbounded_channel();
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

    tx.send(1).unwrap();
    assert_eq!(wakes(&first), 0);
    assert_eq!(wakes(&second), 1);
    assert_eq!(rx.poll_recv(&mut noop_context()), Poll::Ready(Some(1)));

    drop(tx);
    assert_eq!(rx.poll_recv(&mut noop_context()), Poll::Ready(None));
}

#[test]
fn the_last_sender_dropping_wakes_a_pending_receiver() {
    let (tx, mut rx) = mpsc::unbounded_channel::<u8>();
    let mut recv = Task::new(rx.recv());
    recv.pending();
    drop(tx);
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), None);
}

#[test]
fn a_recv_future_dropped_early_takes_nothing() {
    let (tx, mut rx) = mpsc::unbounded_channel();

    let mut waiting = Task::new(rx.recv());
    waiting.pending();
    drop(waiting);

    tx.send(7).unwrap();
    assert_eq!(block_on(rx.recv()), Some(7));
}

#[test]
fn blocking_recv_waits_for_a_send_from_another_thread() {
    let (tx, mut rx) = mpsc::unbounded_channel::<u8>();
    let receiver = std::thread::spawn(move || {
        assert_eq!(rx.blocking_recv(), Some(1));
        assert_eq!(rx.blocking_recv(), None);
    });
    tx.send(1).unwrap();
    drop(tx);
    receiver.join().unwrap();
}

#[test]
fn closed_completes_for_every_sender_when_the_receiver_drops() {
    let (tx1, rx) = mpsc::unbounded_channel::<()>();
    let tx2 = tx1.clone();
    let mut first = Task::new(tx1.closed());
    let mut second = Task::new(tx2.closed());
    first.pending();
    second.pending();
    let dropper = std::thread::spawn(move || drop(rx));
    dropper.join().unwrap();
    assert!(first.is_woken() && second.is_woken());
    first.ready();
    second.ready();
}

#[test]
fn debug_output_names_the_handles_and_shows_no_payload() {
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(String::from("secret")).unwrap();
    let sender = format!("{tx:?}");
    let receiver = format!("{rx:?}");
    let weak = format!("{:?}", tx.downgrade());
    assert!(sender.starts_with("UnboundedSender"), "{sender}");
    assert!(receiver.starts_with("UnboundedReceiver"), "{receiver}");
    assert!(weak.starts_with("WeakUnboundedSender"), "{weak}");
    for text in [sender, receiver, weak] {
        assert!(!text.contains("secret"), "{text}");
    }
}
