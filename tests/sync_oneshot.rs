//! Tests of `nuclei::sync::oneshot`: the behaviour of the established async
//! channel API it follows, checked call by call; drop and cancel safety (a value is dropped
//! exactly once on every path); and threads racing every pair of operations.
#![cfg(not(target_arch = "wasm32"))]

mod sync_support;

use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nuclei::sync::oneshot::error::{RecvError, TryRecvError};
use nuclei::sync::oneshot::{self, Receiver};
use sync_support::{Drops, Probe, Task, block_on};

fn poll_rx<T>(probe: &mut Probe, rx: &mut Receiver<T>) -> std::task::Poll<Result<T, RecvError>> {
    probe.enter(|cx| Pin::new(rx).poll(cx))
}

#[test]
fn send_recv() {
    let (tx, rx) = oneshot::channel();
    let mut rx = Task::new(rx);
    rx.pending();
    assert!(tx.send(1).is_ok());
    assert!(rx.is_woken());
    assert_eq!(rx.ready(), Ok(1));
}

#[test]
fn async_send_recv() {
    let (tx, rx) = oneshot::channel();
    assert!(tx.send(1).is_ok());
    assert_eq!(block_on(rx), Ok(1));
}

#[test]
fn a_value_sent_before_the_first_poll_is_returned_by_it() {
    let (tx, mut rx) = oneshot::channel();
    assert!(tx.send("early").is_ok());
    let mut probe = Probe::new();
    assert_eq!(
        poll_rx(&mut probe, &mut rx),
        std::task::Poll::Ready(Ok("early"))
    );
    assert_eq!(probe.wakes(), 0, "nothing was registered, nothing woken");
}

#[test]
fn close_tx() {
    let (tx, rx) = oneshot::channel::<i32>();
    let mut rx = Task::new(rx);
    rx.pending();
    drop(tx);
    assert!(rx.is_woken());
    assert!(rx.ready().is_err());
}

/// A `RecvError` to compare with: the type has no public constructor.
struct RecvErrorProbe;

impl RecvErrorProbe {
    fn get() -> RecvError {
        let (tx, rx) = oneshot::channel::<()>();
        drop(tx);
        block_on(rx).expect_err("the sender was dropped")
    }
}

#[test]
fn close_rx() {
    // Without ever asking whether it is closed.
    let (tx, _) = oneshot::channel();
    assert!(tx.send(1).is_err());

    // Through poll_closed.
    let (tx, rx) = oneshot::channel::<i32>();
    let mut tx = Some(tx);
    let mut probe = Probe::new();
    assert!(
        probe
            .enter(|cx| tx.as_mut().unwrap().poll_closed(cx))
            .is_pending()
    );
    drop(rx);
    assert!(probe.is_woken());
    assert!(tx.as_ref().unwrap().is_closed());
    assert!(
        probe
            .enter(|cx| tx.as_mut().unwrap().poll_closed(cx))
            .is_ready()
    );
    assert_eq!(tx.take().unwrap().send(1), Err(1));
}

#[test]
fn async_rx_closed() {
    let (mut tx, rx) = oneshot::channel::<()>();
    let closer = std::thread::spawn(move || drop(rx));
    block_on(tx.closed());
    closer.join().expect("closer");
}

#[test]
fn explicit_close_poll() {
    let mut probe = Probe::new();

    // A message sent before the close is still delivered.
    let (tx, mut rx) = oneshot::channel();
    assert!(tx.send(1).is_ok());
    rx.close();
    assert_eq!(poll_rx(&mut probe, &mut rx), std::task::Poll::Ready(Ok(1)));

    // Closed with nothing sent: the sender is woken and told, its send comes
    // back, and the receiver completes with an error.
    let (mut tx, mut rx) = oneshot::channel::<i32>();
    let mut tx_probe = Probe::new();
    assert!(tx_probe.enter(|cx| tx.poll_closed(cx)).is_pending());
    rx.close();
    assert!(tx_probe.is_woken());
    assert!(tx.is_closed());
    assert!(tx_probe.enter(|cx| tx.poll_closed(cx)).is_ready());
    assert_eq!(tx.send(1), Err(1));
    assert!(matches!(
        poll_rx(&mut probe, &mut rx),
        std::task::Poll::Ready(Err(_))
    ));

    // Closed with the sender dropped: the same error.
    let (tx, mut rx) = oneshot::channel::<i32>();
    rx.close();
    drop(tx);
    assert!(matches!(
        poll_rx(&mut probe, &mut rx),
        std::task::Poll::Ready(Err(_))
    ));
}

#[test]
fn explicit_close_try_recv() {
    let (tx, mut rx) = oneshot::channel();
    assert!(tx.send(1).is_ok());
    rx.close();
    assert_eq!(rx.try_recv(), Ok(1));

    let (mut tx, mut rx) = oneshot::channel::<i32>();
    let mut probe = Probe::new();
    assert!(probe.enter(|cx| tx.poll_closed(cx)).is_pending());
    rx.close();
    assert!(probe.is_woken());
    assert!(tx.is_closed());
    assert!(probe.enter(|cx| tx.poll_closed(cx)).is_ready());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
}

#[test]
#[should_panic(expected = "called after complete")]
fn close_try_recv_poll() {
    let (_tx, mut rx) = oneshot::channel::<i32>();
    rx.close();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    let mut probe = Probe::new();
    let _ = poll_rx(&mut probe, &mut rx);
}

#[test]
fn close_after_recv() {
    let (tx, mut rx) = oneshot::channel::<i32>();
    assert!(tx.send(17).is_ok());
    assert_eq!(rx.try_recv(), Ok(17));
    rx.close();
    rx.close();
}

#[test]
fn closing_twice_is_the_same_as_once() {
    let (tx, mut rx) = oneshot::channel::<i32>();
    rx.close();
    rx.close();
    assert!(tx.is_closed());
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
}

#[test]
fn try_recv_after_completion() {
    let (tx, mut rx) = oneshot::channel::<i32>();
    assert!(tx.send(17).is_ok());
    assert_eq!(rx.try_recv(), Ok(17));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    rx.close();
}

#[test]
fn try_recv_after_completion_await() {
    let (tx, rx) = oneshot::channel::<i32>();
    let mut rx = Some(rx);
    assert!(tx.send(17).is_ok());
    let mut probe = Probe::new();
    assert_eq!(
        poll_rx(&mut probe, rx.as_mut().unwrap()),
        std::task::Poll::Ready(Ok(17))
    );
    let mut rx = rx.unwrap();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    rx.close();
}

#[test]
fn try_recv_reports_empty_closed_and_value() {
    let (tx, mut rx) = oneshot::channel::<i32>();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    assert!(!rx.is_terminated(), "empty is not terminal");
    assert!(tx.send(3).is_ok());
    assert_eq!(rx.try_recv(), Ok(3));
    assert!(rx.is_terminated());

    let (tx, mut rx) = oneshot::channel::<i32>();
    drop(tx);
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    assert!(rx.is_terminated());
}

#[test]
fn drops_tasks() {
    let (mut tx, mut rx) = oneshot::channel::<i32>();
    let mut tx_task = Probe::new();
    let mut rx_task = Probe::new();

    assert!(tx_task.enter(|cx| tx.poll_closed(cx)).is_pending());
    assert!(rx_task.enter(|cx| Pin::new(&mut rx).poll(cx)).is_pending());

    drop(tx);
    drop(rx);

    assert_eq!(tx_task.waker_ref_count(), 1);
    assert_eq!(rx_task.waker_ref_count(), 1);
}

#[test]
fn receiver_changes_task() {
    let (tx, mut rx) = oneshot::channel();
    let mut task1 = Probe::new();
    let mut task2 = Probe::new();

    assert!(task1.enter(|cx| Pin::new(&mut rx).poll(cx)).is_pending());
    assert_eq!(task1.waker_ref_count(), 2);
    assert_eq!(task2.waker_ref_count(), 1);

    assert!(task2.enter(|cx| Pin::new(&mut rx).poll(cx)).is_pending());
    assert_eq!(task1.waker_ref_count(), 1, "the first waker was released");
    assert_eq!(task2.waker_ref_count(), 2);

    assert!(tx.send(1).is_ok());
    assert!(!task1.is_woken());
    assert!(task2.is_woken());
    assert!(task2.enter(|cx| Pin::new(&mut rx).poll(cx)).is_ready());
}

#[test]
fn sender_changes_task() {
    let (mut tx, rx) = oneshot::channel::<i32>();
    let mut task1 = Probe::new();
    let mut task2 = Probe::new();

    assert!(task1.enter(|cx| tx.poll_closed(cx)).is_pending());
    assert_eq!(task1.waker_ref_count(), 2);
    assert_eq!(task2.waker_ref_count(), 1);

    assert!(task2.enter(|cx| tx.poll_closed(cx)).is_pending());
    assert_eq!(task1.waker_ref_count(), 1);
    assert_eq!(task2.waker_ref_count(), 2);

    drop(rx);
    assert!(!task1.is_woken());
    assert!(task2.is_woken());
    assert!(task2.enter(|cx| tx.poll_closed(cx)).is_ready());
}

#[test]
fn receiver_is_terminated_send() {
    let (tx, rx) = oneshot::channel::<i32>();
    let mut rx = Some(rx);
    assert!(!rx.as_ref().unwrap().is_terminated());
    assert!(tx.send(17).is_ok());
    assert!(
        !rx.as_ref().unwrap().is_terminated(),
        "a value that was sent but not read does not terminate it"
    );
    let mut probe = Probe::new();
    assert_eq!(
        poll_rx(&mut probe, rx.as_mut().unwrap()),
        std::task::Poll::Ready(Ok(17))
    );
    assert!(rx.unwrap().is_terminated());
}

#[test]
fn receiver_is_terminated_drop_and_close() {
    let (tx, mut rx) = oneshot::channel::<i32>();
    assert!(!rx.is_terminated());
    drop(tx);
    assert!(!rx.is_terminated(), "not before the error was read");
    let mut probe = Probe::new();
    assert!(matches!(
        poll_rx(&mut probe, &mut rx),
        std::task::Poll::Ready(Err(_))
    ));
    assert!(rx.is_terminated());

    let (_tx, mut rx) = oneshot::channel::<i32>();
    rx.close();
    assert!(!rx.is_terminated());
    assert!(matches!(
        poll_rx(&mut probe, &mut rx),
        std::task::Poll::Ready(Err(_))
    ));
    assert!(rx.is_terminated());
}

#[test]
fn receiver_is_empty() {
    let (tx, mut rx) = oneshot::channel::<i32>();
    assert!(rx.is_empty());
    assert!(tx.send(17).is_ok());
    assert!(!rx.is_empty());
    assert_eq!(rx.try_recv(), Ok(17));
    assert!(rx.is_empty(), "empty after the value was read");

    let (tx, mut rx) = oneshot::channel::<i32>();
    drop(tx);
    assert!(rx.is_empty(), "a dropped sender leaves it empty");
    assert!(rx.try_recv().is_err());
    assert!(rx.is_empty());

    let (_tx, mut rx) = oneshot::channel::<i32>();
    rx.close();
    assert!(rx.is_empty());
}

#[test]
fn a_value_sent_before_close_keeps_the_channel_non_empty() {
    let (tx, mut rx) = oneshot::channel::<i32>();
    assert!(tx.send(5).is_ok());
    rx.close();
    assert!(!rx.is_empty());
    assert_eq!(rx.try_recv(), Ok(5));
}

#[test]
fn a_send_after_close_leaves_the_channel_empty_and_returns_the_value() {
    let (tx, mut rx) = oneshot::channel::<i32>();
    rx.close();
    assert_eq!(tx.send(5), Err(5));
    assert!(
        rx.is_empty(),
        "the failed send is not visible to the receiver"
    );
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
}

#[test]
fn blocking_recv_gets_the_value_or_the_error() {
    let (tx, rx) = oneshot::channel::<u8>();
    let sender = std::thread::spawn(move || tx.send(10));
    assert_eq!(rx.blocking_recv(), Ok(10));
    assert!(sender.join().expect("sender").is_ok());

    let (tx, rx) = oneshot::channel::<u8>();
    let dropper = std::thread::spawn(move || drop(tx));
    assert!(rx.blocking_recv().is_err());
    dropper.join().expect("dropper");

    let (tx, rx) = oneshot::channel::<u8>();
    assert!(tx.send(1).is_ok());
    assert_eq!(rx.blocking_recv(), Ok(1), "a value already there");
}

#[test]
#[should_panic(expected = "called after complete")]
fn blocking_recv_of_a_completed_receiver_panics() {
    let (tx, mut rx) = oneshot::channel::<u8>();
    assert!(tx.send(1).is_ok());
    assert_eq!(rx.try_recv(), Ok(1));
    let _ = rx.blocking_recv();
}

#[test]
fn polling_a_pending_receiver_by_mutable_reference_is_cancel_safe() {
    let (tx, mut rx) = oneshot::channel();
    {
        let mut once = Task::new(&mut rx);
        once.pending();
        // Dropped while pending, as a select does with the losing branch.
    }
    assert!(tx.send(9).is_ok());
    let mut again = Task::new(&mut rx);
    assert_eq!(again.ready(), Ok(9));
}

#[test]
fn a_pending_sender_closed_future_is_cancel_safe() {
    let (mut tx, rx) = oneshot::channel::<u8>();
    {
        let mut closed = Task::new(tx.closed());
        closed.pending();
    }
    assert!(!tx.is_closed());
    let mut closed = Task::new(tx.closed());
    closed.pending();
    drop(rx);
    assert_eq!(closed.wakes(), 1);
    closed.ready();
}

#[test]
fn a_closed_future_on_an_already_closed_channel_is_ready() {
    let (mut tx, rx) = oneshot::channel::<u8>();
    drop(rx);
    let mut closed = Task::new(tx.closed());
    closed.ready();
}

#[test]
fn errors_display_and_compare_like_the_established_api() {
    assert_eq!(TryRecvError::Empty.to_string(), "channel empty");
    assert_eq!(TryRecvError::Closed.to_string(), "channel closed");
    assert_eq!(RecvErrorProbe::get().to_string(), "channel closed");
    assert_eq!(TryRecvError::Empty, TryRecvError::Empty.clone());
    assert_ne!(TryRecvError::Empty, TryRecvError::Closed);
    assert_eq!(RecvErrorProbe::get(), RecvErrorProbe::get());
    let as_error: &dyn Error = &TryRecvError::Closed;
    assert!(as_error.source().is_none());
    let as_error: &dyn Error = &RecvErrorProbe::get();
    assert!(as_error.source().is_none());
    assert_eq!(format!("{:?}", TryRecvError::Empty), "Empty");
    assert!(format!("{:?}", RecvErrorProbe::get()).starts_with("RecvError"));
}

#[test]
fn debug_prints_without_requiring_the_value_to_be_debug() {
    struct Opaque;
    let (tx, rx) = oneshot::channel::<Opaque>();
    assert!(format!("{tx:?}").contains("Sender"));
    assert!(format!("{rx:?}").contains("Receiver"));
    assert!(format!("{rx:?}").contains("is_empty: true"));
}

// ---- drop safety: a value is dropped exactly once on every path ----

#[test]
fn a_received_value_is_dropped_once_by_whoever_holds_it() {
    let drops = Drops::new();
    let (tx, rx) = oneshot::channel();
    assert!(tx.send(drops.value()).is_ok());
    let got = block_on(rx).expect("sent");
    assert_eq!(drops.count(), 0);
    drop(got);
    assert_eq!(drops.count(), 1);
}

#[test]
fn a_value_nobody_received_is_dropped_with_the_receiver() {
    let drops = Drops::new();
    let (tx, rx) = oneshot::channel();
    assert!(tx.send(drops.value()).is_ok());
    assert_eq!(drops.count(), 0, "it stays in the channel");
    drop(rx);
    assert_eq!(drops.count(), 1);
}

#[test]
fn a_value_sent_before_close_is_dropped_by_the_receiver_drop() {
    let drops = Drops::new();
    let (tx, mut rx) = oneshot::channel();
    assert!(tx.send(drops.value()).is_ok());
    rx.close();
    assert_eq!(drops.count(), 0);
    drop(rx);
    assert_eq!(drops.count(), 1);
}

#[test]
fn a_value_that_could_not_be_sent_comes_back_and_is_dropped_by_the_caller() {
    let drops = Drops::new();
    let (tx, rx) = oneshot::channel();
    drop(rx);
    let back = tx.send(drops.value()).expect_err("the receiver is gone");
    assert_eq!(drops.count(), 0);
    drop(back);
    assert_eq!(drops.count(), 1);
}

#[test]
fn a_dropped_sender_drops_nothing() {
    let drops = Drops::new();
    let (tx, rx) = oneshot::channel::<sync_support::Counted>();
    drop(tx);
    drop(rx);
    assert_eq!(drops.count(), 0);
}

#[test]
fn a_value_taken_by_try_recv_is_not_dropped_by_the_receiver() {
    let drops = Drops::new();
    let (tx, mut rx) = oneshot::channel();
    assert!(tx.send(drops.value()).is_ok());
    let got = rx.try_recv().expect("sent");
    drop(rx);
    assert_eq!(drops.count(), 0);
    drop(got);
    assert_eq!(drops.count(), 1);
}

// ---- threads ----

fn rounds() -> usize {
    if cfg!(miri) { 20 } else { 20_000 }
}

/// Send racing a close of the receiver: either the send succeeded and the
/// receiver owns the value (it drops it, or reads it), or it failed and the
/// sender got the value back. Never both, never neither.
#[test]
fn a_send_racing_a_receiver_close_has_one_owner() {
    for _ in 0..rounds() {
        let drops = Drops::new();
        let (tx, mut rx) = oneshot::channel();
        let sender = {
            let value = drops.value();
            std::thread::spawn(move || tx.send(value).err())
        };
        rx.close();
        let received = rx.try_recv();
        let back = sender.join().expect("sender");
        match (received, back) {
            (Ok(value), None) => drop(value),
            (Err(TryRecvError::Closed), Some(value)) => drop(value),
            (Err(TryRecvError::Empty), _) => panic!("closed, and still empty"),
            (received, back) => panic!(
                "two owners or none: received {}, returned {}",
                received.is_ok(),
                back.is_some()
            ),
        }
        drop(rx);
        assert_eq!(drops.count(), 1);
    }
}

/// Send racing the drop of the receiver.
#[test]
fn a_send_racing_a_receiver_drop_drops_the_value_once() {
    for _ in 0..rounds() {
        let drops = Drops::new();
        let (tx, rx) = oneshot::channel();
        let value = drops.value();
        let sender = std::thread::spawn(move || tx.send(value).is_ok());
        drop(rx);
        let sent = sender.join().expect("sender");
        assert_eq!(drops.count(), 1, "sent: {sent}");
    }
}

/// The sender dropped racing a poll: the receiver ends with the error and is
/// woken if it waited.
#[test]
fn a_sender_drop_racing_a_receiver_wait_wakes_it() {
    for _ in 0..rounds() {
        let (tx, rx) = oneshot::channel::<u8>();
        let dropper = std::thread::spawn(move || drop(tx));
        assert!(block_on(rx).is_err());
        dropper.join().expect("dropper");
    }
}

/// Many senders on many threads: every value arrives exactly once.
#[test]
fn many_channels_across_threads_deliver_every_value_once() {
    let delivered = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let delivered = Arc::clone(&delivered);
            std::thread::spawn(move || {
                for i in 0..if cfg!(miri) { 20 } else { 2_000 } {
                    let (tx, rx) = oneshot::channel();
                    let sender = std::thread::spawn(move || tx.send((t, i)).is_ok());
                    let got = block_on(rx).expect("the sender sends");
                    assert_eq!(got, (t, i));
                    assert!(sender.join().expect("sender"));
                    delivered.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("worker");
    }
    assert_eq!(
        delivered.load(Ordering::SeqCst),
        8 * if cfg!(miri) { 20 } else { 2_000 }
    );
}

/// The sender waiting for the close, and the receiver closing from another
/// thread: the sender is always woken.
#[test]
fn a_close_racing_a_closed_wait_wakes_the_sender() {
    for _ in 0..rounds() {
        let (mut tx, mut rx) = oneshot::channel::<u8>();
        let closer = std::thread::spawn(move || rx.close());
        block_on(tx.closed());
        closer.join().expect("closer");
        assert!(tx.is_closed());
    }
}

/// A value too large for the atomics is still delivered whole across threads.
#[test]
fn large_values_arrive_intact() {
    for round in 0..if cfg!(miri) { 5 } else { 200 } {
        let (tx, rx) = oneshot::channel();
        let sender = std::thread::spawn(move || tx.send([round; 64]).is_ok());
        let got = block_on(rx).expect("sent");
        assert!(got.iter().all(|word| *word == round));
        assert!(sender.join().expect("sender"));
    }
}

#[test]
fn the_receiver_is_a_future_that_can_be_awaited_in_an_async_block() {
    let (tx, rx) = oneshot::channel();
    let mut task = Task::new(async move { rx.await.map(|v: u8| v + 1) });
    task.pending();
    assert!(tx.send(1).is_ok());
    assert_eq!(task.wakes(), 1);
    assert_eq!(task.ready(), Ok(2));
}

#[test]
fn closed_is_an_async_fn_that_can_race_a_computation() {
    let (mut tx, rx) = oneshot::channel::<u8>();
    drop(rx);
    let mut task = Task::new(async move {
        tx.closed().await;
        tx.send(1)
    });
    assert_eq!(task.ready(), Err(1));
}

// ---- a sequential model ----

mod model {
    use super::*;
    use proptest::prelude::*;
    use std::task::Poll;

    #[derive(Clone, Debug)]
    enum Op {
        Send,
        DropTx,
        TxIsClosed,
        TxPollClosed,
        Close,
        TryRecv,
        Poll,
        IsEmpty,
        IsTerminated,
        DropRx,
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        let op = prop_oneof![
            3 => Just(Op::Send),
            1 => Just(Op::DropTx),
            2 => Just(Op::TxIsClosed),
            2 => Just(Op::TxPollClosed),
            2 => Just(Op::Close),
            3 => Just(Op::TryRecv),
            3 => Just(Op::Poll),
            2 => Just(Op::IsEmpty),
            2 => Just(Op::IsTerminated),
            1 => Just(Op::DropRx),
        ];
        proptest::collection::vec(op, 0..40)
    }

    proptest! {
        /// Any sequence of calls on the two halves agrees with a plain
        /// model of the channel, and every value made is dropped exactly
        /// once at the end: received, returned by a failed send, or dropped
        /// with the receiver.
        #[test]
        fn the_channel_agrees_with_a_sequential_model(ops in ops()) {
            let drops = Drops::new();
            let mut made = 0usize;
            let (tx, rx) = oneshot::channel::<sync_support::Counted>();
            let mut tx = Some(tx);
            let mut rx = Some(rx);
            let mut tx_probe = Probe::new();
            let mut rx_probe = Probe::new();
            // The model: is a value waiting, has the receiver closed (or
            // dropped), is the sender gone, and has the receiver completed.
            let mut waiting = false;
            let mut closed = false;
            let mut tx_gone = false;
            let mut done = false;
            let mut held = Vec::new();

            for op in ops {
                match op {
                    Op::Send => {
                        if let Some(sender) = tx.take() {
                            made += 1;
                            match sender.send(drops.value()) {
                                Ok(()) => {
                                    prop_assert!(!closed, "a send succeeded after the close");
                                    waiting = true;
                                }
                                Err(back) => {
                                    prop_assert!(closed, "a send failed while the receiver was open");
                                    held.push(back);
                                }
                            }
                            tx_gone = true;
                        }
                    }
                    Op::DropTx => {
                        if tx.take().is_some() {
                            tx_gone = true;
                        }
                    }
                    Op::TxIsClosed => {
                        if let Some(sender) = &tx {
                            prop_assert_eq!(sender.is_closed(), closed);
                        }
                    }
                    Op::TxPollClosed => {
                        if let Some(sender) = &mut tx {
                            let ready = tx_probe.enter(|cx| sender.poll_closed(cx)).is_ready();
                            prop_assert_eq!(ready, closed);
                        }
                    }
                    Op::Close => {
                        if let Some(receiver) = &mut rx {
                            receiver.close();
                            if !done {
                                closed = true;
                            }
                        }
                    }
                    Op::TryRecv => {
                        if let Some(receiver) = &mut rx {
                            let got = receiver.try_recv();
                            if done {
                                prop_assert_eq!(got.err(), Some(TryRecvError::Closed));
                            } else if waiting {
                                prop_assert!(got.is_ok());
                                waiting = false;
                                done = true;
                            } else if closed || tx_gone {
                                prop_assert_eq!(got.err(), Some(TryRecvError::Closed));
                                done = true;
                            } else {
                                prop_assert_eq!(got.err(), Some(TryRecvError::Empty));
                            }
                        }
                    }
                    Op::Poll => {
                        if let Some(receiver) = &mut rx
                            && !done
                        {
                            let polled = poll_rx(&mut rx_probe, receiver);
                            if waiting {
                                prop_assert!(matches!(polled, Poll::Ready(Ok(_))));
                                waiting = false;
                                done = true;
                            } else if closed || tx_gone {
                                prop_assert!(matches!(polled, Poll::Ready(Err(_))));
                                done = true;
                            } else {
                                prop_assert!(polled.is_pending());
                            }
                        }
                    }
                    Op::IsEmpty => {
                        if let Some(receiver) = &rx {
                            prop_assert_eq!(receiver.is_empty(), !waiting);
                        }
                    }
                    Op::IsTerminated => {
                        if let Some(receiver) = &rx {
                            prop_assert_eq!(receiver.is_terminated(), done);
                        }
                    }
                    Op::DropRx => {
                        if rx.take().is_some() {
                            closed = true;
                            waiting = false;
                        }
                    }
                }
            }
            drop(tx);
            drop(rx);
            drop(held);
            prop_assert_eq!(drops.count(), made, "every value made was dropped once");
        }
    }
}
