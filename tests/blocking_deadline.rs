//! `blocking_recv_deadline` on the mpsc receivers and the oneshot receiver: a value before, at
//! and after the deadline, a close before it, many senders, a timeout never early, and the same
//! wait inside a task of a shard. Nothing is timed against a tight bound: a wait that must end
//! ends by a value or a close, and each one has a deadline far past what a working wait needs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nuclei::runtime::Builder;
use nuclei::sync::mpsc;
use nuclei::sync::mpsc::error::RecvDeadlineError;
use nuclei::sync::oneshot::{self, error::RecvDeadlineError as OneshotDeadlineError};

const FAR: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_millis(60);

/// The tests of both mpsc flavours, which differ only in how a channel is made and a value sent.
macro_rules! mpsc_flavour {
    ($name:ident, $make:expr, $send:expr) => {
        mod $name {
            use super::*;

            #[test]
            fn a_value_sent_before_the_call_is_received_at_once() {
                let (tx, mut rx) = $make;
                $send(&tx, 5u32);
                let began = Instant::now();
                assert_eq!(rx.blocking_recv_deadline(began + FAR), Ok(5));
                assert!(began.elapsed() < FAR);
            }

            #[test]
            fn a_value_sent_while_it_waits_wakes_it_before_the_deadline() {
                let (tx, mut rx) = $make;
                let sender = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(30));
                    $send(&tx, 9u32);
                    tx
                });
                let began = Instant::now();
                assert_eq!(rx.blocking_recv_deadline(began + FAR), Ok(9));
                assert!(
                    began.elapsed() < FAR,
                    "it woke on the send, not the deadline"
                );
                drop(sender.join().unwrap());
            }

            #[test]
            fn an_empty_open_channel_times_out_and_never_early() {
                let (tx, mut rx) = $make;
                let deadline = Instant::now() + SHORT;
                assert_eq!(
                    rx.blocking_recv_deadline(deadline),
                    Err(RecvDeadlineError::Timeout)
                );
                assert!(
                    Instant::now() >= deadline,
                    "a timeout came before its deadline"
                );
                // The receiver waits again after a timeout.
                $send(&tx, 1u32);
                assert_eq!(rx.blocking_recv_deadline(Instant::now() + FAR), Ok(1));
            }

            #[test]
            fn a_deadline_already_past_still_takes_a_value_that_is_there() {
                let (tx, mut rx) = $make;
                let past = Instant::now();
                assert_eq!(
                    rx.blocking_recv_deadline(past),
                    Err(RecvDeadlineError::Timeout)
                );
                $send(&tx, 3u32);
                assert_eq!(rx.blocking_recv_deadline(past), Ok(3));
            }

            #[test]
            fn a_close_before_the_deadline_ends_the_wait_after_the_buffered_values() {
                let (tx, mut rx) = $make;
                $send(&tx, 1u32);
                let closer = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(30));
                    drop(tx);
                });
                let began = Instant::now();
                assert_eq!(rx.blocking_recv_deadline(began + FAR), Ok(1));
                assert_eq!(
                    rx.blocking_recv_deadline(began + FAR),
                    Err(RecvDeadlineError::Disconnected)
                );
                assert!(began.elapsed() < FAR, "the close woke it, not the deadline");
                closer.join().unwrap();
            }

            #[test]
            fn many_senders_race_a_deadline_receiver_and_no_value_is_lost() {
                const SENDERS: u32 = 8;
                const EACH: u32 = 500;
                let (tx, mut rx) = $make;
                let threads: Vec<_> = (0..SENDERS)
                    .map(|_| {
                        let tx = tx.clone();
                        std::thread::spawn(move || (0..EACH).for_each(|n| $send(&tx, n)))
                    })
                    .collect();
                drop(tx);
                let deadline = Instant::now() + FAR;
                let mut got = 0u64;
                loop {
                    match rx.blocking_recv_deadline(deadline) {
                        Ok(n) => got += u64::from(n),
                        Err(RecvDeadlineError::Disconnected) => break,
                        Err(RecvDeadlineError::Timeout) => panic!("timed out with senders live"),
                    }
                }
                let each: u64 = (0..u64::from(EACH)).sum();
                assert_eq!(got, each * u64::from(SENDERS));
                threads.into_iter().for_each(|t| t.join().unwrap());
            }

            #[test]
            fn a_value_that_lands_as_the_deadline_passes_is_not_lost() {
                // Many short waits, each racing one send: every round ends in the value or a
                // timeout, and a timeout leaves the value for the next receive.
                let (tx, mut rx) = $make;
                for round in 0..200u32 {
                    let tx = tx.clone();
                    let sender = std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_micros(u64::from(round % 7) * 300));
                        $send(&tx, round);
                    });
                    let mut next =
                        rx.blocking_recv_deadline(Instant::now() + Duration::from_micros(500));
                    if next == Err(RecvDeadlineError::Timeout) {
                        next = rx.blocking_recv_deadline(Instant::now() + FAR);
                    }
                    assert_eq!(next, Ok(round));
                    sender.join().unwrap();
                }
            }

            #[test]
            fn the_wait_inside_a_task_of_a_shard_ends_by_value_close_and_deadline() {
                let rt = Builder::new().worker_threads(1).build().unwrap();
                let (tx, mut rx) = $make;
                let task = rt.spawn(async move {
                    let t = Instant::now();
                    assert_eq!(
                        rx.blocking_recv_deadline(t + SHORT),
                        Err(RecvDeadlineError::Timeout)
                    );
                    assert!(t.elapsed() >= SHORT);
                    assert_eq!(rx.blocking_recv_deadline(Instant::now() + FAR), Ok(4));
                    assert_eq!(
                        rx.blocking_recv_deadline(Instant::now() + FAR),
                        Err(RecvDeadlineError::Disconnected)
                    );
                });
                std::thread::sleep(SHORT * 2);
                $send(&tx, 4u32);
                drop(tx);
                rt.block_on(task);
            }
        }
    };
}

mpsc_flavour!(
    bounded,
    mpsc::channel::<u32>(16),
    |tx: &mpsc::Sender<u32>, n| tx.blocking_send(n).unwrap()
);
mpsc_flavour!(
    unbounded,
    mpsc::unbounded_channel::<u32>(),
    |tx: &mpsc::UnboundedSender<u32>, n| tx.send(n).unwrap()
);

#[test]
fn oneshot_value_before_at_and_after_the_deadline() {
    let (tx, mut rx) = oneshot::channel::<u8>();
    tx.send(1).unwrap();
    assert_eq!(rx.blocking_recv_deadline(Instant::now() + FAR), Ok(1));
    // Spent: later calls say closed, and do not panic.
    assert_eq!(
        rx.blocking_recv_deadline(Instant::now() + FAR),
        Err(OneshotDeadlineError::Closed)
    );

    let (tx, mut rx) = oneshot::channel::<u8>();
    let deadline = Instant::now() + SHORT;
    assert_eq!(
        rx.blocking_recv_deadline(deadline),
        Err(OneshotDeadlineError::Timeout)
    );
    assert!(
        Instant::now() >= deadline,
        "a timeout came before its deadline"
    );
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        tx.send(2).unwrap();
    });
    let began = Instant::now();
    assert_eq!(rx.blocking_recv_deadline(began + FAR), Ok(2));
    assert!(began.elapsed() < FAR);
    sender.join().unwrap();

    let (tx, mut rx) = oneshot::channel::<u8>();
    let past = Instant::now();
    tx.send(3).unwrap();
    assert_eq!(rx.blocking_recv_deadline(past), Ok(3));
}

#[test]
fn oneshot_close_before_the_deadline_ends_the_wait() {
    let (tx, mut rx) = oneshot::channel::<u8>();
    let closer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        drop(tx);
    });
    let began = Instant::now();
    assert_eq!(
        rx.blocking_recv_deadline(began + FAR),
        Err(OneshotDeadlineError::Closed)
    );
    assert!(began.elapsed() < FAR);
    closer.join().unwrap();
}

#[test]
fn oneshot_value_racing_a_short_deadline_is_never_lost() {
    for round in 0..200u8 {
        let (tx, mut rx) = oneshot::channel::<u8>();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_micros(u64::from(round % 7) * 300));
            tx.send(round).unwrap();
        });
        let mut got = rx.blocking_recv_deadline(Instant::now() + Duration::from_micros(500));
        if got == Err(OneshotDeadlineError::Timeout) {
            got = rx.blocking_recv_deadline(Instant::now() + FAR);
        }
        assert_eq!(got, Ok(round));
        sender.join().unwrap();
    }
}

#[test]
fn oneshot_wait_inside_a_task_of_a_shard() {
    let rt = Builder::new().worker_threads(1).build().unwrap();
    let (tx, mut rx) = oneshot::channel::<u8>();
    let ready = Arc::new(AtomicBool::new(false));
    let task = {
        let ready = Arc::clone(&ready);
        rt.spawn(async move {
            assert_eq!(
                rx.blocking_recv_deadline(Instant::now() + SHORT),
                Err(OneshotDeadlineError::Timeout)
            );
            ready.store(true, Ordering::Release);
            assert_eq!(rx.blocking_recv_deadline(Instant::now() + FAR), Ok(8));
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(5));
    }
    tx.send(8).unwrap();
    rt.block_on(task);
}
