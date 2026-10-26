//! Loom models for the multi-producer, single-consumer channels of
//! `nuclei::sync::mpsc`.
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --release --test sync_loom_mpsc
//! ```
//!
//! Under `--cfg loom` the atomics, the wait queues, the unsafe cells and the
//! waker cell the channels are built from are loom's instrumented ones, so each
//! model runs the protocol under every interleaving loom reaches within the
//! preemption bound. What the models prove:
//!
//! - **No lost wakeup.** Every wait goes through a `block_on` that parks the
//!   loom thread until its waker fires; a wakeup that is never delivered leaves
//!   every thread parked and loom reports the deadlock. The calibration
//!   `calibration_a_poll_that_skips_the_recheck_loses_a_wakeup` writes the
//!   receiver's poll without the re-check after the registration and must fail
//!   that way.
//! - **A value still being written is waited for.** A send claims its place
//!   in the queue, then writes its value, then wakes the receiver; a receiver
//!   that finds the place claimed and empty registers and returns `Pending`,
//!   and must be woken by that send. The models of a send racing the
//!   receiver's wait count the looks that found a value still being written
//!   and fail unless their search reached one.
//! - **A value has one owner.** Every model counts the drops of the values it
//!   sends, on a counter loom does not track and the model reads only after
//!   every thread joined: each value is dropped exactly once, whoever ends up
//!   holding it (the receiver, a refused sender, the channel).
//! - **A closed channel finishes.** A receiver that closes the channel and then
//!   receives until `None` gets every value a send accepted, and a reservation
//!   or a send that raced the close either got its value in or got it back.
//! - **The count of senders is exact.** A weak sender's upgrade racing the last
//!   strong sender's drop either wins, and the channel then stays open until
//!   the new sender goes, or loses and finds the channel ended.
//! - **The bounded channel's waiters are served in order, and a waiter that
//!   leaves passes its share on.** A release racing two waiting reservations
//!   serves the older one. A reservation dropped while a release grants it
//!   (whole, or the rest of a share set aside for it) leaves its slots to the
//!   reservation behind it, which is woken and served, and the capacity ends
//!   whole. A close racing a grant ends the wait exactly once, either way,
//!   and every slot comes back. Each of these models also counts the places
//!   the semaphore handed a share on (a wait granted then dropped, a waiter
//!   that withdrew as it was granted, a set-aside returned to the free count)
//!   and fails unless its search reached the ones it is about.
//!
//! What the models do not cover. The message queue is kovan's segment queue,
//! which loom cannot instrument; under `--cfg loom` the channel puts a loom
//! `Mutex` around a deque of cells behind the same type (see
//! `src/sync/mpsc/queue.rs`; the cancellation token's registry is modelled the
//! same way). A push there is two steps, a claim and a write, as in the
//! segment queue. So the models check the protocol around the queue: the
//! waker handshake (a value still being written included), the sender count,
//! the closed bit and the limit's idle test. The queue's own order, progress
//! and memory reclamation are kovan's, checked by its TLA+ model and its
//! tests.
//!
//! Every calibration expects a failure the protocol does not allow, or an
//! outcome the model must be able to reach, and must fail: without them a
//! search that never reached the bad interleaving would pass for the same
//! reason a correct one does. The fair semaphore's calibrations switch on a
//! mutation inside the crate (`loom_model::fair_drop_pass_on`) that drops a
//! leaving waiter's share instead of handing it on, and must fail.

#![cfg(loom)]

use std::future::Future;
use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};
use std::task::{Context, Poll};

use loom::sync::Notify as Park;
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::thread;

use nuclei::sync::atomic_waker::AtomicWaker;
use nuclei::sync::loom_model::{fair_drop_pass_on, mpsc_busy_looks};
use nuclei::sync::mpsc::{
    self,
    error::{SendError, TrySendError},
};

/// Runs `model` under loom with at most `preemptions` preemptions per
/// schedule (`LOOM_MAX_PREEMPTIONS` overrides it) and fails it if the search
/// was implausibly small.
fn explore(
    name: &str,
    preemptions: usize,
    min_interleavings: usize,
    model: impl Fn() + Sync + Send + 'static,
) {
    let runs = std::sync::Arc::new(StdAtomicUsize::new(0));
    let counted = {
        let runs = std::sync::Arc::clone(&runs);
        move || {
            model();
            runs.fetch_add(1, StdOrdering::Relaxed);
        }
    };
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = builder.preemption_bound.or(Some(preemptions));
    builder.check(counted);
    let explored = runs.load(StdOrdering::Relaxed);
    println!("loom model {name}: {explored} interleavings explored");
    assert!(
        explored >= min_interleavings,
        "{name} explored only {explored} interleavings, below the {min_interleavings} floor"
    );
}

/// [`explore`], then fails the model unless its search found the next value
/// still being written at least once.
fn explore_busy(
    name: &str,
    preemptions: usize,
    min_interleavings: usize,
    model: impl Fn() + Sync + Send + 'static,
) {
    mpsc_busy_looks();
    explore(name, preemptions, min_interleavings, model);
    let busy = mpsc_busy_looks();
    println!("loom model {name}: {busy} looks found the next value still being written");
    assert!(
        busy > 0,
        "{name}: the search never found a value still being written"
    );
}

struct Unpark(Park);

impl std::task::Wake for Unpark {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.notify();
    }

    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        self.0.notify();
    }
}

/// Drives `future` on this loom thread, parking it between polls until its
/// waker fires. A wakeup that never comes parks it for good, which loom
/// reports as a deadlock.
///
/// It yields before it parks: a waker that fired during the poll (the waker
/// cell wakes a registration that met a wake in flight) lets the park return
/// at once, and a thread that polls again and again without ever blocking
/// would keep the thread that is mid-wake from running, which loom reports as
/// a model that never ends.
fn block_on<F: Future>(future: F) -> F::Output {
    let park = std::sync::Arc::new(Unpark(Park::new()));
    let waker = std::task::Waker::from(std::sync::Arc::clone(&park));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        thread::yield_now();
        park.0.wait();
    }
}

/// A value that counts its drops on a plain atomic, which loom does not
/// track: the count is read only after every thread was joined.
struct Counted(std::sync::Arc<StdAtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, StdOrdering::SeqCst);
    }
}

/// A drop counter and a factory for values that count into it.
struct Drops(std::sync::Arc<StdAtomicUsize>);

impl Drops {
    fn new() -> Self {
        Self(std::sync::Arc::new(StdAtomicUsize::new(0)))
    }

    fn value(&self) -> Counted {
        Counted(std::sync::Arc::clone(&self.0))
    }

    fn count(&self) -> usize {
        self.0.load(StdOrdering::SeqCst)
    }
}

// ---- a send racing the receiver's wait ----

#[test]
fn a_bounded_send_racing_the_receivers_wait_is_delivered_once() {
    explore_busy(
        "a_bounded_send_racing_the_receivers_wait_is_delivered_once",
        3,
        100,
        || {
            let drops = Drops::new();
            let (tx, mut rx) = mpsc::channel::<Counted>(1);
            let value = drops.value();
            let sender = thread::spawn(move || block_on(tx.send(value)).is_ok());
            let got = block_on(rx.recv()).expect("the sender sends");
            assert!(sender.join().expect("sender"));
            drop(got);
            assert_eq!(drops.count(), 1);
        },
    );
}

#[test]
fn an_unbounded_send_racing_the_receivers_wait_is_delivered_once() {
    explore_busy(
        "an_unbounded_send_racing_the_receivers_wait_is_delivered_once",
        3,
        100,
        || {
            let drops = Drops::new();
            let (tx, mut rx) = mpsc::unbounded_channel::<Counted>();
            let value = drops.value();
            let sender = thread::spawn(move || tx.send(value).is_ok());
            let got = block_on(rx.recv()).expect("the sender sends");
            assert!(sender.join().expect("sender"));
            drop(got);
            assert_eq!(drops.count(), 1);
        },
    );
}

/// Two senders, one receiver waiting for both values: neither wake is lost,
/// and each value arrives once.
#[test]
fn two_senders_racing_one_waiting_receiver_both_arrive() {
    explore_busy(
        "two_senders_racing_one_waiting_receiver_both_arrive",
        2,
        3000,
        || {
            let drops = Drops::new();
            let (tx, mut rx) = mpsc::unbounded_channel::<Counted>();
            let tx2 = tx.clone();
            let (a, b) = (drops.value(), drops.value());
            let first = thread::spawn(move || tx.send(a).is_ok());
            let second = thread::spawn(move || tx2.send(b).is_ok());
            let one = block_on(rx.recv());
            let two = block_on(rx.recv());
            assert!(one.is_some() && two.is_some());
            assert!(first.join().expect("first") && second.join().expect("second"));
            assert_eq!(block_on(rx.recv()).map(drop), None);
            drop((one, two));
            assert_eq!(drops.count(), 2);
        },
    );
}

#[test]
fn recv_many_racing_a_send_takes_the_value_in_one_batch() {
    explore(
        "recv_many_racing_a_send_takes_the_value_in_one_batch",
        3,
        800,
        || {
            let drops = Drops::new();
            let (tx, mut rx) = mpsc::channel::<Counted>(2);
            let (a, b) = (drops.value(), drops.value());
            let sender = thread::spawn(move || {
                tx.try_send(a).map_err(|_| ()).expect("room for the first");
                tx.try_send(b).map_err(|_| ()).expect("room for the second");
            });
            let mut batch = Vec::new();
            let mut total = 0;
            while total < 2 {
                let got = block_on(rx.recv_many(&mut batch, 2));
                assert!(got >= 1, "recv_many returns zero only at the end");
                total += got;
            }
            sender.join().expect("sender");
            assert_eq!(batch.len(), 2);
            drop(batch);
            assert_eq!(drops.count(), 2);
        },
    );
}

// ---- the last sender dropping ----

#[test]
fn the_last_sender_dropping_racing_a_waiting_recv_ends_it() {
    explore(
        "the_last_sender_dropping_racing_a_waiting_recv_ends_it",
        3,
        500,
        || {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            let dropper = thread::spawn(move || drop(tx));
            assert_eq!(block_on(rx.recv()), None);
            dropper.join().expect("dropper");

            let (tx, mut rx) = mpsc::unbounded_channel::<u8>();
            let dropper = thread::spawn(move || drop(tx));
            assert_eq!(block_on(rx.recv()), None);
            dropper.join().expect("dropper");
        },
    );
}

#[test]
fn a_value_sent_before_the_last_sender_drops_is_received_before_the_end() {
    explore(
        "a_value_sent_before_the_last_sender_drops_is_received_before_the_end",
        3,
        250,
        || {
            let drops = Drops::new();
            let (tx, mut rx) = mpsc::unbounded_channel::<Counted>();
            let value = drops.value();
            let sender = thread::spawn(move || {
                tx.send(value).map_err(|_| ()).expect("receiver is alive");
            });
            let got = block_on(rx.recv());
            assert!(got.is_some(), "the value comes before the end");
            assert!(block_on(rx.recv()).is_none());
            sender.join().expect("sender");
            drop(got);
            assert_eq!(drops.count(), 1);
        },
    );
}

#[test]
fn the_last_of_two_senders_dropping_ends_the_channel_exactly_once() {
    explore(
        "the_last_of_two_senders_dropping_ends_the_channel_exactly_once",
        3,
        2000,
        || {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            let tx2 = tx.clone();
            let first = thread::spawn(move || drop(tx));
            let second = thread::spawn(move || drop(tx2));
            assert_eq!(block_on(rx.recv()), None);
            first.join().expect("first");
            second.join().expect("second");
            assert_eq!(rx.sender_strong_count(), 0);
        },
    );
}

// ---- the receiver dropping ----

/// A bounded send waits for room in a full channel; the receiver drops. The
/// send fails with its value, and nothing leaks.
#[test]
fn a_receiver_drop_fails_a_waiting_bounded_send_and_returns_its_value() {
    explore(
        "a_receiver_drop_fails_a_waiting_bounded_send_and_returns_its_value",
        3,
        4000,
        || {
            let drops = Drops::new();
            let (tx, rx) = mpsc::channel::<Counted>(1);
            tx.try_send(drops.value())
                .map_err(|_| ())
                .expect("the first fits");
            let second = drops.value();
            let sender = thread::spawn(move || block_on(tx.send(second)));
            drop(rx);
            let SendError(back) = sender
                .join()
                .expect("sender")
                .expect_err("the channel is full and then closed");
            assert!(
                drops.count() <= 1,
                "the queued value is dropped by the receiver, the refused one is still held"
            );
            drop(back);
            assert_eq!(drops.count(), 2, "both values dropped exactly once");
        },
    );
}

#[test]
fn an_unbounded_send_racing_the_receiver_drop_is_refused_or_dropped_with_the_channel() {
    explore(
        "an_unbounded_send_racing_the_receiver_drop_is_refused_or_dropped_with_the_channel",
        3,
        30,
        || {
            let drops = Drops::new();
            let (tx, rx) = mpsc::unbounded_channel::<Counted>();
            let value = drops.value();
            let sender = thread::spawn(move || tx.send(value).map_err(|e| e.0));
            drop(rx);
            let outcome = sender.join().expect("sender");
            // Accepted: queued behind the drop, dropped when the channel is.
            // Refused: handed back. Either way, once.
            drop(outcome);
            assert_eq!(drops.count(), 1);
        },
    );
}

#[test]
fn a_waiting_closed_wait_is_woken_by_the_receiver_drop() {
    explore(
        "a_waiting_closed_wait_is_woken_by_the_receiver_drop",
        3,
        1500,
        || {
            let (tx, rx) = mpsc::channel::<u8>(1);
            let waiter = thread::spawn(move || block_on(tx.closed()));
            drop(rx);
            waiter.join().expect("waiter");
        },
    );
}

// ---- permits ----

/// The receiver closed with a reservation outstanding, and waits for the
/// channel to go idle; the permit drops on another thread. The receiver's wait
/// ends.
#[test]
fn a_permit_dropping_wakes_a_closed_receiver_waiting_for_idle() {
    explore(
        "a_permit_dropping_wakes_a_closed_receiver_waiting_for_idle",
        3,
        100,
        || {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            // A sender stays, so the channel can only end by going idle: the
            // permit's drop is the one thing that can wake the receiver.
            let permit = tx.clone().try_reserve_owned().expect("the slot is free");
            rx.close();
            let dropper = thread::spawn(move || drop(permit));
            assert_eq!(block_on(rx.recv()), None);
            dropper.join().expect("dropper");
            drop(tx);
        },
    );
}

#[test]
fn a_permit_sending_wakes_a_closed_receiver_which_gets_the_value() {
    explore(
        "a_permit_sending_wakes_a_closed_receiver_which_gets_the_value",
        3,
        100,
        || {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            let permit = tx.try_reserve_owned().expect("the slot is free");
            rx.close();
            let sender = thread::spawn(move || drop(permit.send(7)));
            assert_eq!(block_on(rx.recv()), Some(7));
            assert_eq!(block_on(rx.recv()), None);
            sender.join().expect("sender");
        },
    );
}

/// The slot is granted to the waiter directly, with no retry or withdrawal
/// after its wake, so the path has few atomic steps: the search is about 7,000
/// interleavings at three preemptions.
#[test]
fn a_permit_dropping_racing_a_waiting_sender_hands_it_the_slot() {
    explore(
        "a_permit_dropping_racing_a_waiting_sender_hands_it_the_slot",
        3,
        5000,
        || {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            let permit = tx.clone().try_reserve_owned().expect("the slot is free");
            let waiter = thread::spawn(move || block_on(tx.send(5)).is_ok());
            drop(permit);
            assert_eq!(block_on(rx.recv()), Some(5));
            assert!(waiter.join().expect("waiter"));
        },
    );
}

// ---- closing ----

/// A `try_send` races the receiver's close. If it was accepted the value is
/// received before the end; if it was refused the value comes back. The
/// receiver that closes then receives to the end sees exactly the accepted
/// ones.
#[test]
fn a_try_send_racing_close_is_received_or_refused_never_lost() {
    explore(
        "a_try_send_racing_close_is_received_or_refused_never_lost",
        3,
        150,
        || {
            let drops = Drops::new();
            let (tx, mut rx) = mpsc::channel::<Counted>(2);
            let value = drops.value();
            let sender = thread::spawn(move || tx.try_send(value));
            rx.close();
            let mut received = 0;
            while let Some(got) = block_on(rx.recv()) {
                received += 1;
                drop(got);
            }
            match sender.join().expect("sender") {
                Ok(()) => assert_eq!(received, 1, "an accepted value is received"),
                Err(TrySendError::Closed(back)) => {
                    assert_eq!(received, 0, "a refused value is not also received");
                    drop(back);
                }
                Err(TrySendError::Full(_)) => panic!("the channel has room"),
            }
            assert_eq!(drops.count(), 1, "exactly once");
        },
    );
}

#[test]
fn an_unbounded_send_racing_close_is_received_or_refused_never_lost() {
    explore(
        "an_unbounded_send_racing_close_is_received_or_refused_never_lost",
        3,
        60,
        || {
            let drops = Drops::new();
            let (tx, mut rx) = mpsc::unbounded_channel::<Counted>();
            let value = drops.value();
            let sender = thread::spawn(move || tx.send(value).map_err(|e| e.0));
            rx.close();
            let mut received = 0;
            while let Some(got) = block_on(rx.recv()) {
                received += 1;
                drop(got);
            }
            match sender.join().expect("sender") {
                Ok(()) => assert_eq!(received, 1, "an accepted value is received"),
                Err(back) => {
                    assert_eq!(received, 0);
                    drop(back);
                }
            }
            assert_eq!(drops.count(), 1, "exactly once");
        },
    );
}

/// A reservation racing the close either holds a slot it can still send
/// through, which the receiver then receives, or fails.
#[test]
fn a_reservation_racing_close_sends_through_or_fails() {
    explore(
        "a_reservation_racing_close_sends_through_or_fails",
        3,
        500,
        || {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            let reserver = thread::spawn(move || match block_on(tx.reserve()) {
                Ok(permit) => {
                    permit.send(3);
                    true
                }
                Err(_) => false,
            });
            rx.close();
            let mut received = Vec::new();
            while let Some(got) = block_on(rx.recv()) {
                received.push(got);
            }
            let sent = reserver.join().expect("reserver");
            assert_eq!(received, if sent { vec![3] } else { vec![] });
        },
    );
}

#[test]
fn a_waiting_reservation_racing_close_and_a_recv_ends_cleanly() {
    explore(
        "a_waiting_reservation_racing_close_and_a_recv_ends_cleanly",
        3,
        25000,
        || {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            tx.try_send(1).expect("the first fits");
            let waiting = thread::spawn(move || block_on(tx.send(2)).is_ok());
            // Make room, then close: the waiting send may take the slot (and its
            // value is then received), or lose to the close (and fails).
            assert_eq!(block_on(rx.recv()), Some(1));
            rx.close();
            let mut rest = Vec::new();
            while let Some(got) = block_on(rx.recv()) {
                rest.push(got);
            }
            let sent = waiting.join().expect("waiting sender");
            assert_eq!(rest, if sent { vec![2] } else { vec![] });
        },
    );
}

// ---- weak senders ----

#[test]
fn an_upgrade_racing_the_last_strong_drop_wins_cleanly_or_finds_the_end() {
    explore(
        "an_upgrade_racing_the_last_strong_drop_wins_cleanly_or_finds_the_end",
        3,
        50,
        || {
            let (tx, mut rx) = mpsc::unbounded_channel::<u8>();
            let weak = tx.downgrade();
            let upgrader = thread::spawn(move || {
                let upgraded = weak.upgrade();
                if let Some(sender) = &upgraded {
                    sender.send(1).expect("receiver is alive");
                }
                upgraded.is_some()
            });
            drop(tx);
            // Either way the receiver ends: the upgrade failed, or the sender
            // it made was dropped at the end of the closure after its send.
            let mut got = Vec::new();
            while let Some(value) = block_on(rx.recv()) {
                got.push(value);
            }
            let upgraded = upgrader.join().expect("upgrader");
            assert_eq!(got, if upgraded { vec![1] } else { vec![] });
            assert_eq!(rx.sender_strong_count(), 0);
        },
    );
}

#[path = "sync_loom_mpsc/fair.rs"]
mod fair;
use fair::poll_once;

// ---- calibration ----

/// The receiver's poll without the re-check after the registration: a send
/// that lands between its first look and its registration is neither seen nor
/// woken.
///
/// Its shared state lives in `std` `Arc`s: loom reports the deadlock by
/// panicking in the model's main thread, and a loom `Arc` dropped while that
/// panic unwinds touches a scheduler that is already gone and aborts the
/// process.
#[test]
#[should_panic(expected = "deadlock")]
fn calibration_a_poll_that_skips_the_recheck_loses_a_wakeup() {
    explore(
        "calibration_a_poll_that_skips_the_recheck_loses_a_wakeup",
        3,
        1,
        || {
            let cell = std::sync::Arc::new(AtomicWaker::new());
            let queued = std::sync::Arc::new(AtomicUsize::new(0));
            let sender = {
                let (cell, queued) = (std::sync::Arc::clone(&cell), std::sync::Arc::clone(&queued));
                thread::spawn(move || {
                    queued.fetch_add(1, Ordering::AcqRel);
                    cell.wake();
                })
            };
            let mut looked = false;
            block_on(std::future::poll_fn(|cx| {
                if !looked {
                    looked = true;
                    if queued.load(Ordering::Acquire) > 0 {
                        return Poll::Ready(());
                    }
                    // The channel registers here and looks again; this one
                    // does not.
                    cell.register(cx.waker());
                    return Poll::Pending;
                }
                Poll::Ready(())
            }));
            sender.join().expect("sender");
        },
    );
}

/// Both outcomes of the race between a send and the close must be reachable
/// by the search: a model that asserted the send always wins must fail, and
/// so must one that asserted it always loses.
#[test]
#[should_panic(expected = "the close came first")]
fn calibration_the_close_can_win_against_a_try_send() {
    explore(
        "calibration_the_close_can_win_against_a_try_send",
        3,
        1,
        || {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            let outcome = std::sync::Arc::new(AtomicBool::new(true));
            let sender = {
                let outcome = std::sync::Arc::clone(&outcome);
                thread::spawn(move || outcome.store(tx.try_send(1).is_ok(), Ordering::Release))
            };
            rx.close();
            sender.join().expect("sender");
            // A loom `Arc` dropped while the failed assertion unwinds aborts the
            // process, so the channel goes first.
            drop(rx);
            assert!(outcome.load(Ordering::Acquire), "the close came first");
        },
    );
}

#[test]
#[should_panic(expected = "the send came first")]
fn calibration_the_try_send_can_win_against_the_close() {
    explore(
        "calibration_the_try_send_can_win_against_the_close",
        3,
        1,
        || {
            let (tx, mut rx) = mpsc::channel::<u8>(1);
            let outcome = std::sync::Arc::new(AtomicBool::new(false));
            let sender = {
                let outcome = std::sync::Arc::clone(&outcome);
                thread::spawn(move || outcome.store(tx.try_send(1).is_ok(), Ordering::Release))
            };
            rx.close();
            sender.join().expect("sender");
            drop(rx);
            assert!(!outcome.load(Ordering::Acquire), "the send came first");
        },
    );
}

/// The cancellation model above, with the mutation that drops a leaving
/// waiter's share instead of handing it on: a search that reaches a dropped
/// request's grant, or its withdrawal at the grant, finds the reservation
/// behind it never served and the capacity short, and must fail.
#[test]
#[should_panic(expected = "the share of a dropped reservation was lost")]
fn calibration_a_share_that_is_not_handed_on_is_lost() {
    explore(
        "calibration_a_share_that_is_not_handed_on_is_lost",
        3,
        1,
        || {
            fair_drop_pass_on(true);
            let (tx, rx) = mpsc::channel::<u8>(2);
            let held = tx
                .clone()
                .try_reserve_owned()
                .expect("the first slot is free");
            let last = tx
                .clone()
                .try_reserve_owned()
                .expect("the second slot is free");
            let mut many = Box::pin(tx.reserve_many(2));
            let mut one = Box::pin(tx.reserve());
            assert!(poll_once(&mut many).is_pending());
            assert!(poll_once(&mut one).is_pending());
            drop(held);
            let releaser = thread::spawn(move || drop(last));
            drop(many);
            releaser.join().expect("releaser");
            // Every pass has run, so a share handed on has reached `one`.
            let served = poll_once(&mut one).is_ready();
            drop(one);
            let capacity = tx.capacity();
            // A loom `Arc` dropped while the failed assertion unwinds aborts the
            // process, so the channel goes first.
            drop((tx, rx));
            fair_drop_pass_on(false);
            assert!(
                served && capacity == 2,
                "the share of a dropped reservation was lost"
            );
        },
    );
}

/// The close model above, with the same mutation: the close no longer
/// returns the slot set aside for the request it fails, so a search that
/// reaches the close first finds the capacity short, and must fail.
#[test]
#[should_panic(expected = "the close kept a set-aside slot")]
fn calibration_a_close_that_does_not_return_the_set_aside_loses_it() {
    explore(
        "calibration_a_close_that_does_not_return_the_set_aside_loses_it",
        3,
        1,
        || {
            fair_drop_pass_on(true);
            let (tx, mut rx) = mpsc::channel::<u8>(2);
            let held = tx
                .clone()
                .try_reserve_owned()
                .expect("the first slot is free");
            let last = tx
                .clone()
                .try_reserve_owned()
                .expect("the second slot is free");
            let mut many = Box::pin(tx.reserve_many(2));
            assert!(poll_once(&mut many).is_pending());
            drop(held);
            let releaser = thread::spawn(move || drop(last));
            rx.close();
            releaser.join().expect("releaser");
            drop(many);
            let capacity = tx.capacity();
            drop((tx, rx));
            fair_drop_pass_on(false);
            assert_eq!(capacity, 2, "the close kept a set-aside slot");
        },
    );
}
