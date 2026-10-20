//! `recv_many`, `poll_recv_many` and `blocking_recv_many`: the limit, the
//! buffer's capacity, waiting, and the end of the channel.

use super::*;

#[test]
fn a_bounded_recv_many_collects_what_a_sender_thread_produces() {
    let (tx, mut rx) = mpsc::channel(2);
    let mut buffer = Vec::<i32>::with_capacity(3);

    // A limit of zero does not wait, and returns at once.
    assert_eq!(0, block_on(rx.recv_many(&mut buffer, 0)));

    let handle = std::thread::spawn(move || {
        block_on(tx.send(1)).unwrap();
        block_on(tx.send(2)).unwrap();
        block_on(tx.send(7)).unwrap();
        block_on(tx.send(0)).unwrap();
    });

    let limit = 3;
    let mut recv_count = 0usize;
    while recv_count < 4 {
        recv_count += block_on(rx.recv_many(&mut buffer, limit));
        assert_eq!(buffer.len(), recv_count);
    }

    // Compared sorted: values sent while the receiver runs can be delivered out
    // of order (see the ordering notes in the module docs).
    buffer.sort_unstable();
    assert_eq!(vec![0, 1, 2, 7], buffer);
    assert_eq!(0, block_on(rx.recv_many(&mut buffer, limit)));
    handle.join().unwrap();
}

#[test]
fn an_unbounded_recv_many_honours_the_limit_and_reuses_the_buffer() {
    let (tx, mut rx) = mpsc::unbounded_channel::<i32>();

    let mut buffer: Vec<i32> = Vec::new();

    // A limit of zero does not wait, and adds nothing.
    block_on(rx.recv_many(&mut buffer, 0));
    assert_eq!(0, buffer.len());

    tx.send(7).unwrap();
    tx.send(13).unwrap();
    tx.send(100).unwrap();
    tx.send(1002).unwrap();

    block_on(rx.recv_many(&mut buffer, 0));
    assert_eq!(0, buffer.len());

    let mut count = 0;
    while count < 4 {
        count += block_on(rx.recv_many(&mut buffer, 1));
    }
    assert_eq!(count, 4);
    assert_eq!(vec![7, 13, 100, 1002], buffer);
    let final_capacity = buffer.capacity();
    assert!(final_capacity > 0);

    buffer.clear();

    tx.send(5).unwrap();
    tx.send(6).unwrap();
    tx.send(7).unwrap();
    tx.send(2).unwrap();

    // Reuses the existing capacity.
    count = block_on(rx.recv_many(&mut buffer, 32));

    assert_eq!(final_capacity, buffer.capacity());
    assert_eq!(count, 4);
    assert_eq!(vec![5, 6, 7, 2], buffer);

    drop(tx);

    // A closed channel with nothing left returns zero at once.
    assert_eq!(0, block_on(rx.recv_many(&mut buffer, 4)));
    assert!(block_on(rx.recv()).is_none());
}

#[test]
fn the_buffer_grows_only_as_needed_in_the_bounded_channel() {
    let mut buffer: Vec<String> = Vec::with_capacity(9);
    let limit = buffer.capacity();
    let (tx, mut rx) = mpsc::channel(100);

    let mut expected: Vec<String> = (0..limit).map(|x: usize| format!("{x}")).collect();
    for x in expected.clone() {
        block_on(tx.send(x)).unwrap();
    }
    block_on(tx.send("one more".to_string())).unwrap();

    // Receives all but the last value; the capacity is adequate, so the buffer
    // does not grow.
    assert_eq!(
        buffer.capacity(),
        block_on(rx.recv_many(&mut buffer, limit))
    );
    assert_eq!(expected, buffer);
    assert_eq!(limit, buffer.capacity());

    // One more value.
    assert_eq!(1, block_on(rx.recv_many(&mut buffer, limit)));
    assert!(buffer.capacity() > limit);
    expected.push("one more".to_string());
    assert_eq!(expected, buffer);

    let sender = std::thread::spawn(move || {
        block_on(tx.send("final".to_string())).unwrap();
    });

    // The sender may already be gone, but the value it sent is received first.
    assert_eq!(1, block_on(rx.recv_many(&mut buffer, limit)));
    expected.push("final".to_string());
    assert_eq!(expected, buffer);
    sender.join().unwrap();
    // The channel is now closed and `recv_many` returns 0.
    assert_eq!(0, block_on(rx.recv_many(&mut buffer, limit)));
    assert_eq!(expected, buffer);
}

#[test]
fn the_buffer_grows_only_as_needed_in_the_unbounded_channel() {
    let mut buffer: Vec<String> = Vec::with_capacity(9);
    let limit = buffer.capacity();
    let (tx, mut rx) = mpsc::unbounded_channel();

    let mut expected: Vec<String> = (0..limit).map(|x: usize| format!("{x}")).collect();
    for x in expected.clone() {
        tx.send(x).unwrap();
    }
    tx.send("one more".to_string()).unwrap();

    assert_eq!(
        buffer.capacity(),
        block_on(rx.recv_many(&mut buffer, limit))
    );
    assert_eq!(expected, buffer);
    assert_eq!(limit, buffer.capacity());

    assert_eq!(1, block_on(rx.recv_many(&mut buffer, limit)));
    assert!(buffer.capacity() > limit);
    expected.push("one more".to_string());
    assert_eq!(expected, buffer);

    let sender = std::thread::spawn(move || {
        tx.send("final".to_string()).unwrap();
    });

    assert_eq!(1, block_on(rx.recv_many(&mut buffer, limit)));
    expected.push("final".to_string());
    assert_eq!(expected, buffer);
    sender.join().unwrap();
    assert_eq!(0, block_on(rx.recv_many(&mut buffer, limit)));
    assert_eq!(expected, buffer);
}

#[test]
fn recv_many_does_not_reserve_the_limit_up_front() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send(1u8).unwrap();
    let mut buffer = Vec::new();
    // A limit of usize::MAX must not try to allocate it.
    assert_eq!(1, block_on(rx.recv_many(&mut buffer, usize::MAX)));
    assert!(buffer.capacity() < 1024);
}

#[test]
fn recv_many_takes_what_is_ready_and_does_not_wait_for_the_limit() {
    let (tx, mut rx) = mpsc::channel(10);
    tx.try_send(1).unwrap();
    tx.try_send(2).unwrap();
    let mut buffer = Vec::new();
    assert_eq!(2, block_on(rx.recv_many(&mut buffer, 100)));
    assert_eq!(buffer, [1, 2]);
}

#[test]
fn recv_many_waits_when_empty_and_is_woken_by_a_send() {
    let (tx, mut rx) = mpsc::channel(10);
    let mut buffer = Vec::new();
    let mut waiting = Task::new(rx.recv_many(&mut buffer, 5));
    waiting.pending();
    tx.try_send(9).unwrap();
    assert!(waiting.is_woken());
    assert_eq!(waiting.ready(), 1);
    drop(waiting);
    assert_eq!(buffer, [9]);
}

#[test]
fn recv_many_gives_every_slot_back_in_one_batch() {
    let (tx, mut rx) = mpsc::channel(4);
    for i in 0..4 {
        tx.try_send(i).unwrap();
    }
    assert_eq!(tx.capacity(), 0);
    let mut buffer = Vec::new();
    assert_eq!(3, block_on(rx.recv_many(&mut buffer, 3)));
    assert_eq!(tx.capacity(), 3);
    assert_eq!(rx.len(), 1);
}

#[test]
fn recv_many_wakes_senders_waiting_for_the_slots_it_frees() {
    let (tx, mut rx) = mpsc::channel(2);
    tx.try_send(1).unwrap();
    tx.try_send(2).unwrap();
    let mut both = Task::new(tx.reserve_many(2));
    both.pending();
    let mut buffer = Vec::new();
    assert_eq!(2, block_on(rx.recv_many(&mut buffer, 2)));
    assert!(both.is_woken());
    assert_eq!(both.ready().unwrap().len(), 2);
}

#[test]
fn a_recv_many_future_dropped_early_takes_nothing() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut buffer = Vec::new();

    let mut waiting = Task::new(rx.recv_many(&mut buffer, 4));
    waiting.pending();
    drop(waiting);

    tx.send(1).unwrap();
    tx.send(2).unwrap();
    assert!(buffer.is_empty());
    assert_eq!(2, block_on(rx.recv_many(&mut buffer, 4)));
}

#[test]
fn recv_many_drains_the_values_left_by_a_closed_channel_then_returns_zero() {
    let (tx, mut rx) = mpsc::channel(8);
    for i in 0..5 {
        tx.try_send(i).unwrap();
    }
    rx.close();
    let mut buffer = Vec::new();
    assert_eq!(5, block_on(rx.recv_many(&mut buffer, 10)));
    assert_eq!(0, block_on(rx.recv_many(&mut buffer, 10)));
    assert_eq!(buffer, [0, 1, 2, 3, 4]);
}

#[test]
fn poll_recv_many_reports_pending_ready_and_the_end() {
    let (tx, mut rx) = mpsc::channel(8);
    let mut buffer = Vec::new();
    let (counter, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);

    assert_eq!(rx.poll_recv_many(&mut cx, &mut buffer, 4), Poll::Pending);
    tx.try_send(1).unwrap();
    tx.try_send(2).unwrap();
    assert_eq!(
        wakes(&counter),
        1,
        "the waker cell is emptied by the first wake"
    );

    assert_eq!(rx.poll_recv_many(&mut cx, &mut buffer, 1), Poll::Ready(1));
    assert_eq!(rx.poll_recv_many(&mut cx, &mut buffer, 0), Poll::Ready(0));
    assert_eq!(rx.poll_recv_many(&mut cx, &mut buffer, 4), Poll::Ready(1));
    assert_eq!(buffer, [1, 2]);

    assert_eq!(rx.poll_recv_many(&mut cx, &mut buffer, 4), Poll::Pending);
    drop(tx);
    assert_eq!(wakes(&counter), 2);
    assert_eq!(rx.poll_recv_many(&mut cx, &mut buffer, 4), Poll::Ready(0));
}

#[test]
fn blocking_recv_many_parks_until_a_batch_arrives() {
    let (tx, mut rx) = mpsc::channel(4);
    let receiver = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut total = 0;
        loop {
            let got = rx.blocking_recv_many(&mut buffer, 3);
            if got == 0 {
                break;
            }
            assert!(got <= 3);
            total += got;
        }
        (total, buffer)
    });
    for i in 0..20 {
        tx.blocking_send(i).unwrap();
    }
    drop(tx);
    let (total, mut buffer) = receiver.join().unwrap();
    assert_eq!(total, 20);
    buffer.sort_unstable();
    assert_eq!(buffer, (0..20).collect::<Vec<_>>());

    let (tx, mut rx) = mpsc::unbounded_channel();
    let receiver = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        while rx.blocking_recv_many(&mut buffer, 8) != 0 {}
        buffer
    });
    for i in 0..20 {
        tx.send(i).unwrap();
    }
    drop(tx);
    let mut buffer = receiver.join().unwrap();
    buffer.sort_unstable();
    assert_eq!(buffer, (0..20).collect::<Vec<_>>());
}
