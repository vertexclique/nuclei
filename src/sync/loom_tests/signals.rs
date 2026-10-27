//! Loom models for the waker cell, the one-shot channel, the watch channel and
//! the cancellation token of `crate::sync`.
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --release --lib -- sync::loom_tests::signals
//! ```
//!
//! Under `--cfg loom` the atomics, the unsafe cells and the wait queues these
//! types are built from are loom's instrumented ones, so each model runs the
//! protocol under every interleaving loom reaches within the preemption
//! bound. What the models prove:
//!
//! - **No lost wakeup.** Every wait goes through a `block_on` that parks the
//!   loom thread until its waker fires; a wakeup that is never delivered
//!   leaves every thread parked and loom reports the deadlock. The
//!   calibration `calibration_a_poll_that_skips_the_recheck_loses_a_wakeup`
//!   writes the receiver's poll without the re-check after the registration
//!   and must fail that way.
//! - **A value has one owner.** The one-shot value cell is a loom
//!   `UnsafeCell`, so two parties touching it at once fail the model as a
//!   data race, and every model counts the drops of the value: exactly one,
//!   whoever ends up holding it.
//! - **A child of a cancelled token is cancelled.** Each cancellation model
//!   joins every thread and then checks every token it made.
//!
//! What the models do not cover. The watch channel keeps its value in a
//! kovan `Atom` and the cancellation token's real registry is a kovan map;
//! loom cannot instrument either, so the models see them as atomic black
//! boxes. For the watch channel that leaves the version, the closed bit, the
//! wake handshake and the counts to the model, and the memory reclamation of
//! the `Atom` to kovan's own tests. For the token, the registry is swapped
//! for a mutex-guarded ordered map under loom and `Arc`/`Weak` for a model of
//! them (see `src/sync/cancellation.rs`): the models check the protocol
//! around the registry, and the module docs give the argument for the fences
//! that order the real registry.
//!
//! Every calibration expects a failure the protocol does not allow, and must
//! fail: without them a search that never reached the bad interleaving would
//! pass for the same reason a correct one does.

use std::future::Future;
use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};
use std::task::{Context, Poll};

use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use loom::sync::{Arc, Notify as Park};
use loom::thread;

use crate::sync::CancellationToken;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::oneshot::{self, error::TryRecvError};
use crate::sync::watch;

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

/// A waker that counts its wakes on a loom atomic.
struct Counting(AtomicUsize);

impl std::task::Wake for Counting {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting() -> (std::sync::Arc<Counting>, std::task::Waker) {
    let counter = std::sync::Arc::new(Counting(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(std::sync::Arc::clone(&counter));
    (counter, waker)
}

/// A value that counts its drops on a plain atomic, which loom does not
/// track: the count is read only after every thread was joined.
struct Counted(std::sync::Arc<StdAtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, StdOrdering::SeqCst);
    }
}

fn counted() -> (std::sync::Arc<StdAtomicUsize>, Counted) {
    let drops = std::sync::Arc::new(StdAtomicUsize::new(0));
    (std::sync::Arc::clone(&drops), Counted(drops))
}

// ---- the waker cell ----

#[test]
fn a_register_racing_a_wake_loses_no_wakeup() {
    explore("a_register_racing_a_wake_loses_no_wakeup", 3, 10, || {
        let cell = Arc::new(AtomicWaker::new());
        let ready = Arc::new(AtomicBool::new(false));
        let waking = {
            let (cell, ready) = (Arc::clone(&cell), Arc::clone(&ready));
            thread::spawn(move || {
                ready.store(true, Ordering::Release);
                cell.wake();
            })
        };
        block_on(std::future::poll_fn(|cx| {
            cell.register(cx.waker());
            if ready.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        waking.join().expect("waking thread");
    });
}

/// The same race with the registration repeated on every poll, as a task
/// polled several times does, and a wake that can land between two of them.
#[test]
fn registrations_repeated_against_a_wake_lose_no_wakeup() {
    explore(
        "registrations_repeated_against_a_wake_lose_no_wakeup",
        3,
        10,
        || {
            let cell = Arc::new(AtomicWaker::new());
            let step = Arc::new(AtomicUsize::new(0));
            let waking = {
                let (cell, step) = (Arc::clone(&cell), Arc::clone(&step));
                thread::spawn(move || {
                    step.fetch_add(1, Ordering::AcqRel);
                    cell.wake();
                    step.fetch_add(1, Ordering::AcqRel);
                    cell.wake();
                })
            };
            block_on(std::future::poll_fn(|cx| {
                cell.register(cx.waker());
                if step.load(Ordering::Acquire) == 2 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }));
            waking.join().expect("waking thread");
        },
    );
}

#[test]
fn a_wake_before_the_registration_is_neither_lost_nor_doubled() {
    explore(
        "a_wake_before_the_registration_is_neither_lost_nor_doubled",
        3,
        5,
        || {
            let cell = Arc::new(AtomicWaker::new());
            let (counter, waker) = counting();
            let waking = {
                let cell = Arc::clone(&cell);
                thread::spawn(move || cell.wake())
            };
            cell.register(&waker);
            waking.join().expect("waking thread");
            // The wake took the waker, or came first and took nothing, or landed
            // during the registration and the registrant woke itself. A last wake
            // collects whatever is still registered: exactly one wake in all.
            cell.wake();
            assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn a_new_registration_replaces_the_old_one_under_a_wake() {
    explore(
        "a_new_registration_replaces_the_old_one_under_a_wake",
        3,
        10,
        || {
            let cell = Arc::new(AtomicWaker::new());
            let (first, first_waker) = counting();
            let (second, second_waker) = counting();
            let waking = {
                let cell = Arc::clone(&cell);
                thread::spawn(move || cell.wake())
            };
            cell.register(&first_waker);
            cell.register(&second_waker);
            waking.join().expect("waking thread");
            cell.wake();
            // The wake may take the first waker before the second replaces it;
            // the second is woken exactly once by the wake that finds it, and the
            // first is never woken twice.
            assert_eq!(second.0.load(Ordering::SeqCst), 1);
            assert!(first.0.load(Ordering::SeqCst) <= 1);
        },
    );
}

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
            let ready = std::sync::Arc::new(AtomicBool::new(false));
            let waking = {
                let (cell, ready) = (std::sync::Arc::clone(&cell), std::sync::Arc::clone(&ready));
                thread::spawn(move || {
                    ready.store(true, Ordering::Release);
                    cell.wake();
                })
            };
            let mut looked = false;
            block_on(std::future::poll_fn(|cx| {
                if !looked {
                    looked = true;
                    if ready.load(Ordering::Acquire) {
                        return Poll::Ready(());
                    }
                    cell.register(cx.waker());
                    return Poll::Pending;
                }
                Poll::Ready(())
            }));
            waking.join().expect("waking thread");
        },
    );
}

// ---- the one-shot channel ----

#[test]
fn a_send_racing_the_receivers_wait_is_delivered_once() {
    explore(
        "a_send_racing_the_receivers_wait_is_delivered_once",
        3,
        10,
        || {
            let (drops, value) = counted();
            let (tx, rx) = oneshot::channel();
            let sender = thread::spawn(move || tx.send(value).is_ok());
            let got = block_on(rx).expect("the sender sends");
            assert!(sender.join().expect("sender"));
            drop(got);
            assert_eq!(drops.load(StdOrdering::SeqCst), 1);
        },
    );
}

#[test]
fn a_send_racing_the_receiver_drop_has_one_owner() {
    explore(
        "a_send_racing_the_receiver_drop_has_one_owner",
        3,
        10,
        || {
            let (drops, value) = counted();
            let (tx, rx) = oneshot::channel();
            let sender = thread::spawn(move || tx.send(value).err());
            drop(rx);
            let back = sender.join().expect("sender");
            // Sent and dropped by the receiver, or returned to the sender.
            let returned = usize::from(back.is_some());
            assert_eq!(drops.load(StdOrdering::SeqCst), 1 - returned);
            drop(back);
            assert_eq!(drops.load(StdOrdering::SeqCst), 1);
        },
    );
}

#[test]
fn a_send_racing_a_close_then_try_recv_has_one_owner() {
    explore(
        "a_send_racing_a_close_then_try_recv_has_one_owner",
        3,
        10,
        || {
            let (drops, value) = counted();
            let (tx, mut rx) = oneshot::channel();
            let sender = thread::spawn(move || tx.send(value).err());
            rx.close();
            let received = rx.try_recv();
            let back = sender.join().expect("sender");
            match (received, back) {
                (Ok(value), None) => drop(value),
                (Err(TryRecvError::Closed), Some(value)) => drop(value),
                (received, back) => panic!(
                    "not one owner: received {}, returned {}",
                    received.is_ok(),
                    back.is_some()
                ),
            }
            drop(rx);
            assert_eq!(drops.load(StdOrdering::SeqCst), 1);
        },
    );
}

#[test]
fn a_send_racing_a_close_then_drop_has_one_owner() {
    explore(
        "a_send_racing_a_close_then_drop_has_one_owner",
        3,
        10,
        || {
            let (drops, value) = counted();
            let (tx, mut rx) = oneshot::channel();
            let sender = thread::spawn(move || tx.send(value).err());
            rx.close();
            drop(rx);
            let back = sender.join().expect("sender");
            assert_eq!(
                drops.load(StdOrdering::SeqCst),
                1 - usize::from(back.is_some())
            );
            drop(back);
            assert_eq!(drops.load(StdOrdering::SeqCst), 1);
        },
    );
}

#[test]
fn a_sender_drop_racing_the_receivers_wait_wakes_it() {
    explore(
        "a_sender_drop_racing_the_receivers_wait_wakes_it",
        3,
        5,
        || {
            let (tx, rx) = oneshot::channel::<u8>();
            let dropper = thread::spawn(move || drop(tx));
            assert!(block_on(rx).is_err());
            dropper.join().expect("dropper");
        },
    );
}

#[test]
fn a_sender_drop_racing_a_close_leaves_nothing() {
    explore("a_sender_drop_racing_a_close_leaves_nothing", 3, 2, || {
        let (tx, mut rx) = oneshot::channel::<u8>();
        let dropper = thread::spawn(move || drop(tx));
        rx.close();
        dropper.join().expect("dropper");
        assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    });
}

#[test]
fn a_close_racing_the_senders_closed_wait_wakes_it() {
    explore(
        "a_close_racing_the_senders_closed_wait_wakes_it",
        3,
        10,
        || {
            let (drops, value) = counted();
            let (mut tx, mut rx) = oneshot::channel();
            let sender = thread::spawn(move || {
                block_on(tx.closed());
                tx.send(value).err()
            });
            rx.close();
            let back = sender.join().expect("sender");
            assert!(back.is_some(), "the send comes after the close");
            drop(back);
            drop(rx);
            assert_eq!(drops.load(StdOrdering::SeqCst), 1);
        },
    );
}

#[test]
fn a_receiver_drop_racing_the_senders_closed_wait_wakes_it() {
    explore(
        "a_receiver_drop_racing_the_senders_closed_wait_wakes_it",
        3,
        5,
        || {
            let (mut tx, rx) = oneshot::channel::<u8>();
            let dropper = thread::spawn(move || drop(rx));
            block_on(tx.closed());
            assert!(tx.is_closed());
            dropper.join().expect("dropper");
        },
    );
}

// ---- the watch channel ----

/// The stack of a loom thread is small, and kovan's first use on a thread
/// needs more than it has, so the watch models run their threads on bigger
/// ones.
const BIG_STACK: usize = 4 * 1024 * 1024;

fn spawn_big<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> thread::JoinHandle<T> {
    thread::Builder::new()
        .stack_size(BIG_STACK)
        .spawn(f)
        .expect("spawn a loom thread")
}

/// Runs a model's body on a thread with a big stack.
fn on_big_stack(body: impl FnOnce() + Send + 'static) {
    spawn_big(body).join().expect("the body of the model");
}

#[test]
fn a_send_racing_a_changed_wait_loses_no_wakeup() {
    explore(
        "a_send_racing_a_changed_wait_loses_no_wakeup",
        3,
        10,
        || {
            on_big_stack(|| {
                let (tx, mut rx) = watch::channel(0u32);
                let sender = spawn_big(move || {
                    tx.send(1).expect("the receiver is alive");
                    tx
                });
                block_on(rx.changed()).expect("the sender is alive");
                assert_eq!(*rx.borrow(), 1);
                drop(sender.join().expect("sender"));
            });
        },
    );
}

#[test]
fn the_last_sender_drop_racing_a_changed_wait_wakes_it() {
    explore(
        "the_last_sender_drop_racing_a_changed_wait_wakes_it",
        3,
        5,
        || {
            on_big_stack(|| {
                let (tx, mut rx) = watch::channel(0u32);
                let dropper = spawn_big(move || drop(tx));
                block_on(rx.changed()).expect_err("the only sender is gone");
                dropper.join().expect("dropper");
            });
        },
    );
}

/// A send followed by the drop of the last sender: a receiver that sees the
/// close has seen the version, so it gets the value before the error.
#[test]
fn a_send_then_a_drop_is_seen_in_that_order() {
    explore("a_send_then_a_drop_is_seen_in_that_order", 2, 10, || {
        on_big_stack(|| {
            let (tx, mut rx) = watch::channel(0u32);
            let sender = spawn_big(move || {
                tx.send(1).expect("the receiver is alive");
            });
            block_on(rx.changed()).expect("the send comes first");
            assert_eq!(*rx.borrow_and_update(), 1);
            block_on(rx.changed()).expect_err("then the close");
            sender.join().expect("sender");
        });
    });
}

/// Two receivers waiting and a sender: three threads, so the normal run keeps
/// to one preemption and the twin below runs two.
fn two_receivers_model() {
    on_big_stack(|| {
        let (tx, mut rx1) = watch::channel(0u32);
        let mut rx2 = rx1.clone();
        let first = spawn_big(move || {
            block_on(rx1.changed()).expect("the sender is alive");
            *rx1.borrow()
        });
        let second = spawn_big(move || {
            block_on(rx2.changed()).expect("the sender is alive");
            *rx2.borrow()
        });
        // This thread is the sender.
        tx.send(7).expect("receivers alive");
        assert_eq!(first.join().expect("first receiver"), 7);
        assert_eq!(second.join().expect("second receiver"), 7);
        drop(tx);
    });
}

#[test]
fn two_receivers_waiting_are_both_woken_by_a_send() {
    explore(
        "two_receivers_waiting_are_both_woken_by_a_send",
        1,
        100,
        two_receivers_model,
    );
}

#[test]
#[ignore = "preemption bound 2 takes over ten minutes: run it with --ignored"]
fn two_receivers_waiting_are_both_woken_by_a_send_at_bound_2() {
    explore(
        "two_receivers_waiting_are_both_woken_by_a_send_at_bound_2",
        2,
        100,
        two_receivers_model,
    );
}

#[test]
fn a_wait_for_racing_two_sends_ends_on_the_value_it_waits_for() {
    explore(
        "a_wait_for_racing_two_sends_ends_on_the_value_it_waits_for",
        3,
        10,
        || {
            on_big_stack(|| {
                let (tx, mut rx) = watch::channel(0u32);
                let sender = spawn_big(move || {
                    tx.send(1).expect("alive");
                    tx.send(2).expect("alive");
                    tx
                });
                let seen = *block_on(rx.wait_for(|v| *v == 2)).expect("the sender sends 2");
                assert_eq!(seen, 2);
                drop(sender.join().expect("sender"));
            });
        },
    );
}

#[test]
fn a_receiver_drop_racing_the_producers_closed_wait_wakes_it() {
    explore(
        "a_receiver_drop_racing_the_producers_closed_wait_wakes_it",
        3,
        5,
        || {
            on_big_stack(|| {
                let (tx, rx) = watch::channel(());
                let dropper = spawn_big(move || drop(rx));
                block_on(tx.closed());
                assert!(tx.is_closed());
                dropper.join().expect("dropper");
            });
        },
    );
}

#[test]
fn a_subscribe_racing_the_last_receiver_drop_never_hangs_the_producer() {
    explore(
        "a_subscribe_racing_the_last_receiver_drop_never_hangs_the_producer",
        3,
        10,
        || {
            on_big_stack(|| {
                let (tx, rx) = watch::channel(());
                let tx = Arc::new(tx);
                let reopener = {
                    let tx = Arc::clone(&tx);
                    spawn_big(move || drop(tx.subscribe()))
                };
                drop(rx);
                reopener.join().expect("reopener");
                block_on(tx.closed());
            });
        },
    );
}

// ---- the cancellation token ----

/// The handshake between `child_token` and `cancel`, on bare atomics with the
/// orderings of the real code: the child's side stores that it is registered
/// (a Release store, as the registry's insert is), and then reads the parent's
/// flag; the canceller's side sets the flag, and then reads the registry. With a
/// `SeqCst` fence between the store and the read on each side, one of the two
/// reads sees the other side's store.
fn dekker(fenced: bool) {
    let registered = std::sync::Arc::new(AtomicBool::new(false));
    let cancelled = std::sync::Arc::new(AtomicBool::new(false));
    let canceller = {
        let (registered, cancelled) = (
            std::sync::Arc::clone(&registered),
            std::sync::Arc::clone(&cancelled),
        );
        thread::spawn(move || {
            cancelled.store(true, Ordering::Release);
            if fenced {
                fence(Ordering::SeqCst);
            }
            registered.load(Ordering::Acquire)
        })
    };
    registered.store(true, Ordering::Release);
    if fenced {
        fence(Ordering::SeqCst);
    }
    let child_sees_cancelled = cancelled.load(Ordering::Acquire);
    let canceller_sees_child = canceller.join().expect("canceller");
    assert!(
        child_sees_cancelled || canceller_sees_child,
        "child left running: it missed the cancel and the cancel missed it"
    );
}

#[test]
fn the_fences_of_child_token_and_cancel_make_one_side_see_the_other() {
    explore(
        "the_fences_of_child_token_and_cancel_make_one_side_see_the_other",
        3,
        2,
        || dekker(true),
    );
}

/// Without the fences the handshake fails: the model finds the interleaving
/// where the child misses the cancel and the cancel misses the child.
#[test]
#[should_panic(expected = "child left running")]
fn calibration_a_handshake_without_fences_leaves_a_child_running() {
    explore(
        "calibration_a_handshake_without_fences_leaves_a_child_running",
        3,
        1,
        || dekker(false),
    );
}

#[test]
fn a_cancel_racing_child_token_leaves_the_child_cancelled() {
    explore(
        "a_cancel_racing_child_token_leaves_the_child_cancelled",
        3,
        10,
        || {
            let root = CancellationToken::new();
            let canceller = {
                let root = root.clone();
                thread::spawn(move || root.cancel())
            };
            let child = root.child_token();
            let grandchild = child.child_token();
            canceller.join().expect("canceller");
            assert!(root.is_cancelled());
            assert!(child.is_cancelled());
            assert!(grandchild.is_cancelled());
        },
    );
}

#[test]
fn a_cancel_racing_two_child_tokens_cancels_both() {
    explore(
        "a_cancel_racing_two_child_tokens_cancels_both",
        2,
        10,
        || {
            let root = CancellationToken::new();
            let creator = {
                let root = root.clone();
                thread::spawn(move || root.child_token())
            };
            let canceller = {
                let root = root.clone();
                thread::spawn(move || root.cancel())
            };
            let mine = root.child_token();
            let theirs = creator.join().expect("creator");
            canceller.join().expect("canceller");
            assert!(mine.is_cancelled() && theirs.is_cancelled());
        },
    );
}

#[test]
fn a_cancel_racing_the_drop_of_a_child_is_safe() {
    explore("a_cancel_racing_the_drop_of_a_child_is_safe", 3, 4, || {
        let root = CancellationToken::new();
        let child = root.child_token();
        let kept = child.child_token();
        let canceller = {
            let root = root.clone();
            thread::spawn(move || root.cancel())
        };
        drop(child);
        canceller.join().expect("canceller");
        assert!(
            kept.is_cancelled(),
            "the grandchild outlives the dropped child"
        );
        assert!(root.child_token().is_cancelled());
    });
}

#[test]
fn dropping_the_parent_handle_racing_a_grandparents_cancel_still_reaches_the_child() {
    explore(
        "dropping_the_parent_handle_racing_a_grandparents_cancel_still_reaches_the_child",
        3,
        4,
        || {
            let grandparent = CancellationToken::new();
            let parent = grandparent.child_token();
            let child = parent.child_token();
            let canceller = {
                let grandparent = grandparent.clone();
                thread::spawn(move || grandparent.cancel())
            };
            drop(parent);
            canceller.join().expect("canceller");
            assert!(child.is_cancelled(), "the relay passed the cancel on");
        },
    );
}

#[test]
fn dropping_parent_and_child_racing_a_cancel_frees_every_node() {
    explore(
        "dropping_parent_and_child_racing_a_cancel_frees_every_node",
        2,
        10,
        || {
            let root = CancellationToken::new();
            let parent = root.child_token();
            let child = parent.child_token();
            let canceller = {
                let root = root.clone();
                thread::spawn(move || root.cancel())
            };
            let dropper = thread::spawn(move || {
                drop(parent);
                drop(child);
            });
            canceller.join().expect("canceller");
            dropper.join().expect("dropper");
            assert!(root.is_cancelled());
        },
    );
}

/// Two calls cancel the same parent, which has a child and a grandchild: each
/// returns with all three cancelled, whichever of them marked what.
#[test]
fn two_cancels_racing_on_a_parent_each_return_with_the_subtree_cancelled() {
    explore(
        "two_cancels_racing_on_a_parent_each_return_with_the_subtree_cancelled",
        3,
        10,
        || {
            let root = CancellationToken::new();
            let child = root.child_token();
            let grandchild = child.child_token();
            let other = {
                let (root, child, grandchild) = (root.clone(), child.clone(), grandchild.clone());
                thread::spawn(move || {
                    root.cancel();
                    assert!(
                        root.is_cancelled() && child.is_cancelled() && grandchild.is_cancelled(),
                        "a cancel returned with part of its subtree running"
                    );
                })
            };
            root.cancel();
            assert!(
                root.is_cancelled() && child.is_cancelled() && grandchild.is_cancelled(),
                "a cancel returned with part of its subtree running"
            );
            other.join().expect("other canceller");
        },
    );
}

/// A cancel of the root racing a cancel of its child: whichever marks the child
/// first, the root's call returns with the grandchild cancelled, because it
/// walks below a child another call marked.
#[test]
fn a_cancel_returns_with_the_grandchild_cancelled_whoever_marked_the_child() {
    explore(
        "a_cancel_returns_with_the_grandchild_cancelled_whoever_marked_the_child",
        3,
        10,
        || {
            let root = CancellationToken::new();
            let child = root.child_token();
            let grandchild = child.child_token();
            let lower = {
                let child = child.clone();
                thread::spawn(move || child.cancel())
            };
            root.cancel();
            assert!(
                child.is_cancelled() && grandchild.is_cancelled(),
                "the root's cancel returned with the subtree below it running"
            );
            lower.join().expect("lower canceller");
        },
    );
}

/// The walk on bare atomics: a chain of three flags, one walker starting at the
/// root and one at the middle. With `descend_marked` a walker goes below a flag
/// someone else set; without it, it stops there.
fn walk_chain(descend_marked: bool) {
    let flags: std::sync::Arc<[AtomicBool; 3]> = std::sync::Arc::new([
        AtomicBool::new(false),
        AtomicBool::new(false),
        AtomicBool::new(false),
    ]);
    let walk = move |flags: &[AtomicBool; 3], from: usize| {
        flags[from].store(true, Ordering::Release);
        for next in from + 1..3 {
            let was_set = flags[next].swap(true, Ordering::AcqRel);
            if was_set && !descend_marked {
                return;
            }
        }
    };
    let lower = {
        let flags = std::sync::Arc::clone(&flags);
        thread::spawn(move || walk(&flags, 1))
    };
    walk(&flags, 0);
    assert!(
        flags.iter().all(|f| f.load(Ordering::Acquire)),
        "the walk returned early: part of the chain below it was still running"
    );
    lower.join().expect("lower walker");
}

#[test]
fn a_walk_that_descends_into_marked_nodes_returns_with_the_chain_marked() {
    explore(
        "a_walk_that_descends_into_marked_nodes_returns_with_the_chain_marked",
        3,
        2,
        || walk_chain(true),
    );
}

/// The walk of the previous design, which stopped at a node another call had
/// marked: the model finds the interleaving where it returns before the other
/// call has marked what is below.
#[test]
#[should_panic(expected = "the walk returned early")]
fn calibration_a_walk_that_skips_marked_nodes_returns_early() {
    explore(
        "calibration_a_walk_that_skips_marked_nodes_returns_early",
        3,
        1,
        || walk_chain(false),
    );
}

#[test]
fn a_cancel_racing_a_child_cancel_cancels_each_once() {
    explore(
        "a_cancel_racing_a_child_cancel_cancels_each_once",
        3,
        10,
        || {
            let root = CancellationToken::new();
            let child = root.child_token();
            let grandchild = child.child_token();
            let a = {
                let root = root.clone();
                thread::spawn(move || root.cancel())
            };
            let b = {
                let child = child.clone();
                thread::spawn(move || child.cancel())
            };
            a.join().expect("root canceller");
            b.join().expect("child canceller");
            assert!(root.is_cancelled() && child.is_cancelled() && grandchild.is_cancelled());
        },
    );
}

#[test]
fn a_cancel_wakes_a_waiter_on_a_child_whatever_it_races() {
    explore(
        "a_cancel_wakes_a_waiter_on_a_child_whatever_it_races",
        3,
        10,
        || {
            let root = CancellationToken::new();
            let child = root.child_token();
            let waiter = thread::spawn(move || block_on(child.cancelled()));
            root.cancel();
            waiter.join().expect("waiter");
        },
    );
}

#[test]
fn a_cancel_wakes_an_owned_waiter_on_a_grandchild_whatever_it_races() {
    explore(
        "a_cancel_wakes_an_owned_waiter_on_a_grandchild_whatever_it_races",
        3,
        10,
        || {
            let root = CancellationToken::new();
            let grandchild = root.child_token().child_token();
            let waiter = thread::spawn(move || block_on(grandchild.cancelled_owned()));
            root.cancel();
            waiter.join().expect("waiter");
        },
    );
}
