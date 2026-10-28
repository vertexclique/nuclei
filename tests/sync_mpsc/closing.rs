//! Closing and dropping: `close`, a receiver that waits for a closed channel
//! to go idle, what a held permit changes, `closed()`, and what dropping the
//! receiver does to the queue, the senders' waits and the receiver's waker.

use super::*;

#[test]
fn closing_an_idle_channel_ends_recv_and_fails_sends() {
    let (tx, mut rx) = mpsc::channel::<i32>(10);

    rx.close();

    assert!(block_on(rx.recv()).is_none());

    assert!(block_on(tx.send(1)).is_err());
}

#[test]
fn recv_after_the_last_sender_dropped_returns_none() {
    let (_, mut rx) = mpsc::channel::<i32>(10);
    assert!(block_on(rx.recv()).is_none());
}

#[test]
fn closing_with_a_reservation_held_waits_for_the_permit_before_ending() {
    let (tx1, mut rx) = mpsc::channel::<i32>(1);
    let tx2 = tx1.clone();

    let permit1 = block_on(tx1.reserve()).unwrap();
    let mut permit2 = Task::new(tx2.reserve());
    permit2.pending();

    rx.close();

    // The waiting reservation fails at once.
    assert!(permit2.is_woken());
    assert!(permit2.ready().is_err());

    {
        let mut recv = Task::new(rx.recv());
        recv.pending();

        // The permit held from before the close can still be used.
        permit1.send(123);
        assert!(recv.is_woken());

        assert_eq!(recv.ready(), Some(123));
    }

    assert!(block_on(rx.recv()).is_none());
}

#[test]
fn a_waiting_reservation_failed_by_the_close_can_be_dropped_without_polling_again() {
    let (tx1, mut rx) = mpsc::channel::<()>(1);
    let tx2 = tx1.clone();

    let permit1 = block_on(tx1.reserve()).unwrap();

    let mut permit2 = Task::new(tx2.reserve());
    permit2.pending();

    rx.close();

    drop(permit1);
    assert!(permit2.is_woken());

    drop(permit2);
    assert!(block_on(rx.recv()).is_none());
}

#[test]
fn a_receiver_waiting_on_a_closed_channel_wakes_when_the_last_permit_is_dropped() {
    let (tx, mut rx) = mpsc::channel::<()>(100);
    let _tx2 = tx.clone();

    let permit = block_on(tx.reserve()).unwrap();

    rx.close();

    let mut recv = Task::new(rx.recv());
    recv.pending();

    drop(permit);

    assert!(recv.is_woken());
    assert!(recv.ready().is_none());
}

#[test]
fn dropping_the_last_permit_wakes_a_closed_receiver() {
    let (tx, mut rx) = mpsc::channel::<()>(100);

    let permit = tx.try_reserve().unwrap();
    rx.close();

    let mut recv = Task::new(rx.recv());
    recv.pending();
    drop(permit);
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), None);
}

#[test]
fn dropping_the_last_owned_permit_wakes_a_closed_receiver() {
    let (tx, mut rx) = mpsc::channel::<()>(100);

    // `tx` moves into the permit, so the end is the idle limit's to see.
    let permit = tx.try_reserve_owned().unwrap();
    rx.close();

    let mut recv = Task::new(rx.recv());
    recv.pending();
    drop(permit);
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), None);
}

#[test]
fn dropping_the_last_permit_iterator_wakes_a_closed_receiver() {
    let (tx, mut rx) = mpsc::channel::<()>(100);

    let permits = tx.try_reserve_many(1).unwrap();
    rx.close();

    let mut recv = Task::new(rx.recv());
    recv.pending();
    drop(permits);
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), None);
}

/// A waiting reservation is handed its slot as soon as the slot is free, and
/// holds it before it has polled: nobody else can see that slot. Dropping
/// the reservation then gives the slot back through the semaphore, not
/// through a permit, so the reservation itself must wake a receiver that is
/// waiting for the closed channel to go idle.
#[test]
fn a_handed_slot_returned_by_a_cancelled_reserve_wakes_the_closed_receiver() {
    let (tx, mut rx) = mpsc::channel::<()>(1);
    let held = tx.try_reserve().unwrap();

    let mut waiter = Task::new(tx.reserve());
    waiter.pending();

    drop(held);
    assert!(waiter.is_woken(), "the freed slot was handed to the waiter");
    assert_eq!(tx.capacity(), 0, "and it holds the slot before polling");

    rx.close();
    let mut recv = Task::new(rx.recv());
    recv.pending();

    drop(waiter);
    assert_eq!(tx.capacity(), 1, "the handed slot came back");
    assert!(recv.is_woken(), "and the closed receiver was told");
    assert_eq!(recv.ready(), None);
}

#[test]
fn sending_the_last_permit_wakes_a_closed_receiver() {
    let (tx, mut rx) = mpsc::channel::<()>(100);

    let permit = tx.try_reserve().unwrap();
    rx.close();

    let mut recv = Task::new(rx.recv());
    recv.pending();
    permit.send(());
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), Some(()));
}

#[test]
fn sending_the_last_owned_permit_wakes_a_closed_receiver() {
    let (tx, mut rx) = mpsc::channel::<()>(100);

    let permit = tx.try_reserve_owned().unwrap();
    rx.close();

    let mut recv = Task::new(rx.recv());
    recv.pending();
    let _sender = permit.send(());
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), Some(()));
}

#[test]
fn releasing_the_last_owned_permit_wakes_a_closed_receiver() {
    let (tx, mut rx) = mpsc::channel::<()>(100);

    let permit = tx.try_reserve_owned().unwrap();
    rx.close();

    let mut recv = Task::new(rx.recv());
    recv.pending();
    let inert_sender = permit.release();
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), None);
    drop(inert_sender);
}

#[test]
fn a_reservation_cancelled_after_the_close_wakes_a_receiver_waiting_for_idle() {
    let (tx, mut rx) = mpsc::channel::<()>(2);

    let permit = tx.try_reserve().unwrap();

    let mut reserve = Task::new(tx.reserve_many(2));
    reserve.pending();
    assert_eq!(
        tx.capacity(),
        0,
        "the free slot is set aside for the request"
    );

    rx.close();
    // The close ends the waiting request, and the slot set aside for it
    // comes back with the close (see the module docs, Differences).
    assert!(reserve.is_woken());
    assert_eq!(tx.capacity(), 1);

    let mut recv = Task::new(rx.recv());
    recv.pending();

    // The permit is the last thing outstanding: giving it back ends the
    // receiver's wait.
    drop(permit);
    assert!(recv.is_woken());
    assert!(reserve.ready().is_err());
    drop(reserve);
    assert_eq!(recv.ready(), None);
    assert_eq!(tx.capacity(), 2);
}

#[test]
fn a_waiting_reservation_cancelled_before_the_close_passes_its_share_on() {
    let (tx, mut rx) = mpsc::channel::<u8>(2);

    let permit = tx.try_reserve().unwrap();
    let mut many = Task::new(tx.reserve_many(2));
    many.pending();
    let mut one = Task::new(tx.reserve());
    one.pending();
    assert_eq!(tx.capacity(), 0);

    // The request for two leaves; the slot set aside for it goes on to the
    // reservation behind it.
    drop(many);
    assert!(one.is_woken());
    let second = one.ready().unwrap();

    rx.close();
    permit.send(1);
    second.send(2);
    assert_eq!(block_on(rx.recv()), Some(1));
    assert_eq!(block_on(rx.recv()), Some(2));
    assert_eq!(block_on(rx.recv()), None);
    assert_eq!(tx.capacity(), 2);
}

#[test]
fn a_reservation_failed_by_the_close_leaves_the_receiver_able_to_finish() {
    let (tx, mut rx) = mpsc::channel::<()>(1);

    let permit = tx.try_reserve().unwrap();

    let mut reserve = Task::new(tx.reserve());
    reserve.pending();

    rx.close();
    assert!(reserve.ready().is_err());

    let mut recv = Task::new(rx.recv());
    recv.pending();
    drop(permit);
    assert!(recv.is_woken());
    assert_eq!(recv.ready(), None);
}

#[test]
fn try_recv_after_close_reports_disconnected_once_nothing_is_outstanding() {
    let (_tx, mut rx) = mpsc::channel::<()>(5);

    assert_eq!(Err(TryRecvError::Empty), rx.try_recv());
    rx.close();
    assert_eq!(Err(TryRecvError::Disconnected), rx.try_recv());
}

#[test]
fn try_recv_after_close_with_a_permit_stays_empty_until_the_permit_goes() {
    let (tx, mut rx) = mpsc::channel::<()>(5);

    let permit = tx.try_reserve().unwrap();

    assert_eq!(Err(TryRecvError::Empty), rx.try_recv());
    rx.close();
    assert_eq!(Err(TryRecvError::Empty), rx.try_recv());
    drop(permit);
    assert_eq!(Err(TryRecvError::Disconnected), rx.try_recv());
}

#[test]
fn recv_many_on_a_closed_idle_channel_returns_zero_and_leaves_the_buffer() {
    let (_tx, mut rx) = mpsc::channel::<i32>(1);
    let mut buffer = vec![1];
    rx.close();
    assert_eq!(0, block_on(rx.recv_many(&mut buffer, 1)));
    assert_eq!(vec![1], buffer);

    let (_tx, mut rx) = mpsc::unbounded_channel::<i32>();
    let mut buffer = vec![1];
    rx.close();
    assert_eq!(0, block_on(rx.recv_many(&mut buffer, 1)));
    assert_eq!(vec![1], buffer);
}

#[test]
fn dropping_the_receiver_closes_the_channel_for_every_wait() {
    let (tx, rx) = mpsc::channel(100);

    let msg = Arc::new(());
    tx.try_send(Arc::clone(&msg)).unwrap();

    drop(rx);
    assert!(block_on(tx.reserve()).is_err());
    assert!(block_on(tx.reserve_many(10)).is_err());
    assert_eq!(1, Arc::strong_count(&msg), "the queued value was dropped");
}

#[test]
fn dropping_the_receiver_closes_the_channel_for_every_try() {
    let (tx, rx) = mpsc::channel(100);

    let msg = Arc::new(());
    tx.try_send(Arc::clone(&msg)).unwrap();

    drop(rx);

    assert!(matches!(
        tx.try_send(Arc::clone(&msg)),
        Err(TrySendError::Closed(_))
    ));
    assert!(matches!(tx.try_reserve(), Err(TrySendError::Closed(_))));
    assert!(matches!(
        tx.clone().try_reserve_owned(),
        Err(TrySendError::Closed(_))
    ));

    assert_eq!(1, Arc::strong_count(&msg));
}

#[test]
fn a_waker_dropped_by_the_close_may_return_a_permit_without_deadlock() {
    struct PermitInWaker {
        _permit: mpsc::OwnedPermit<i32>,
    }

    // Does nothing when woken, but owns a permit: what is dropped with it is
    // the point, so `Waker::noop()` would not do.
    #[allow(clippy::manual_noop_waker)]
    impl Wake for PermitInWaker {
        fn wake(self: Arc<Self>) {}
    }

    let (tx, rx) = mpsc::channel(2);
    let permit = tx.clone().try_reserve_owned().unwrap();
    tx.try_send(1).unwrap();

    // The channel is full, so `reserve` waits for capacity.
    let mut reserve = Box::pin(tx.reserve());
    let waker = Waker::from(Arc::new(PermitInWaker { _permit: permit }));
    assert!(
        reserve
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);

    // Closing the channel wakes `reserve`, which drops the last reference to
    // the waker and returns the permit inside it. That must not deadlock.
    drop(rx);

    let mut cx = noop_context();
    assert!(matches!(
        reserve.as_mut().poll(&mut cx),
        Poll::Ready(Err(_))
    ));
}

#[test]
fn dropping_the_receiver_lets_go_of_its_waker() {
    struct DummyWaker;
    // Its references are what the test counts, so `Waker::noop()` would not do.
    #[allow(clippy::manual_noop_waker)]
    impl Wake for DummyWaker {
        fn wake(self: Arc<Self>) {}
    }

    // A dummy `Arc` to count references to the waker.
    let dummy = Arc::new(DummyWaker);
    let waker = Waker::from(Arc::clone(&dummy));
    let mut cx = Context::from_waker(&waker);

    // Two references: `dummy` and `waker`.
    assert_eq!(Arc::strong_count(&dummy), 2);

    // Register `waker` in the receiver.
    let (_tx, mut rx) = mpsc::channel::<()>(1);
    assert!(rx.poll_recv(&mut cx).is_pending());
    assert_eq!(Arc::strong_count(&dummy), 3);

    // Dropping the receiver releases the stored waker.
    drop(rx);
    assert_eq!(
        Arc::strong_count(&dummy),
        2,
        "dropping the receiver did not drop its waker",
    );

    let (_tx, mut rx) = mpsc::unbounded_channel::<()>();
    assert!(rx.poll_recv(&mut cx).is_pending());
    assert_eq!(Arc::strong_count(&dummy), 3);
    drop(rx);
    assert_eq!(Arc::strong_count(&dummy), 2);
}

#[test]
fn closed_completes_for_every_sender_once_the_receiver_drops() {
    let (tx1, rx) = mpsc::channel::<()>(1);
    let others: Vec<_> = (0..4).map(|_| tx1.clone()).collect();
    let mut waits: Vec<_> = others.iter().map(|tx| Task::new(tx.closed())).collect();
    for wait in &mut waits {
        wait.pending();
    }
    let dropper = std::thread::spawn(move || drop(rx));
    dropper.join().unwrap();
    for wait in &mut waits {
        assert!(wait.is_woken());
        wait.ready();
    }
    block_on(tx1.closed());
}

#[test]
fn closed_completes_when_the_receiver_closes_without_dropping() {
    let (tx, mut rx) = mpsc::channel::<()>(1);
    let mut closed = Task::new(tx.closed());
    closed.pending();
    rx.close();
    assert!(closed.is_woken());
    closed.ready();
    // Once closed, always closed.
    block_on(tx.closed());
    block_on(tx.closed());
}

#[test]
fn a_dropped_closed_wait_leaves_nothing_behind() {
    let (tx, rx) = mpsc::channel::<()>(1);
    let mut closed = Task::new(tx.closed());
    closed.pending();
    drop(closed);
    drop(rx);
    block_on(tx.closed());
}

#[test]
fn a_permit_sent_after_the_receiver_dropped_is_dropped_with_the_channel() {
    let drops = Drops::new();
    let (tx, rx) = mpsc::channel(2);
    let permit = tx.try_reserve().unwrap();
    drop(rx);
    // The slot was reserved before the drop, so the send itself goes through;
    // nobody can receive the value, and it is dropped with the channel.
    permit.send(drops.value());
    assert_eq!(drops.count(), 0, "the channel is still held by the sender");
    drop(tx);
    assert_eq!(drops.count(), 1);
}

#[test]
fn closing_twice_and_after_the_senders_are_gone_is_harmless() {
    let (tx, mut rx) = mpsc::channel::<u8>(2);
    tx.try_send(1).unwrap();
    rx.close();
    rx.close();
    drop(tx);
    rx.close();
    assert_eq!(rx.try_recv(), Ok(1));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    assert_eq!(block_on(rx.recv()), None);
}

#[test]
fn clean_shutdown_loses_no_value_a_sender_was_promised() {
    let (tx, mut rx) = mpsc::channel(8);
    let producer = std::thread::spawn(move || {
        let mut i = 0u32;
        // Reserve, then send: a value is only made once its slot is held.
        while let Ok(permit) = block_on(tx.reserve()) {
            permit.send(i);
            i += 1;
        }
        i
    });

    let mut received = vec![block_on(rx.recv()).expect("the producer sends")];
    rx.close();
    while let Some(value) = block_on(rx.recv()) {
        received.push(value);
    }
    let sent = producer.join().unwrap();
    assert_eq!(
        received,
        (0..sent).collect::<Vec<_>>(),
        "every value that got a slot was received, once and in order"
    );
}
