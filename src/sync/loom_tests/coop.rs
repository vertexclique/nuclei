//! Loom models of the cooperative budget's refusals in `crate::sync` (see
//! `crate::coop`): a lock or a send that took what it asked for at its first
//! attempt, and is then refused for its spent budget, gives it back, and a
//! thread that queued for it meanwhile is served.
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --release --lib -- sync::loom_tests::coop
//! ```
//!
//! Loom runs a model's threads on the test's own thread, so they share the
//! budget's thread-local: the models hand the refusal in at the place the
//! budget's answer lands (`contend::admitted`, `FairWait::poll_admitted`),
//! and every other poll runs unbudgeted. A receive refused for the budget
//! looks at it before anything else and touches nothing shared, so its
//! refusal is a poll that did nothing, with no interleaving to check. The
//! models that hold the refusal until the waiter queued, and the send's,
//! count the runs in which the waiter had queued by the time of the refusal
//! and fail unless their search reached one; the lock model that lets the
//! refusal land anywhere checks every other order; the calibration drops the
//! give back and must deadlock.

use std::future::poll_fn;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Waker;

use loom::thread;

use super::mpsc::{block_on, explore};
use crate::sync::contend;
use crate::sync::fair_semaphore::{FairSemaphore, FairWait};
use crate::sync::queue::{Policy, QUEUED};
use crate::sync::raw_semaphore::{PermitWait, RawSemaphore};

/// A lock refused at its first attempt: when the holder thread's take
/// succeeds and the budget says no, the lock goes back through the release's
/// pass, which hands it to a waiter that queued in between. With
/// `until_queued`, the refusal waits until the waiter queued (the waiter's
/// spin before it queues yields to the holder, so otherwise the search meets
/// it only mid-spin); without, it lands wherever loom puts it.
fn lock_refusal(give_back: bool, until_queued: bool, witnessed: Arc<AtomicUsize>) {
    let sem = Arc::new(RawSemaphore::new(1));
    let waited = Arc::new(AtomicBool::new(false));
    let waiter = {
        let (sem, waited) = (Arc::clone(&sem), Arc::clone(&waited));
        thread::spawn(move || {
            let mut wait = PermitWait::new();
            block_on(poll_fn(|cx| {
                let poll = wait.poll_one(&sem, cx.waker());
                if poll.is_pending() {
                    waited.store(true, Ordering::SeqCst);
                }
                poll
            }));
            sem.release(1);
        })
    };
    if sem.take_one().is_ok() {
        if until_queued {
            while sem
                .queue()
                .state
                .load(crate::sync::internal::Ordering::Acquire)
                & QUEUED
                == 0
            {
                thread::yield_now();
            }
        }
        if waited.load(Ordering::SeqCst) {
            witnessed.fetch_add(1, Ordering::SeqCst);
        }
        if give_back {
            assert!(contend::admitted(&*sem, 1, false).is_pending());
        }
    }
    waiter.join().expect("waiter");
    assert_eq!(sem.available(), 1, "a permit was lost or made up");
}

#[test]
fn a_lock_refused_for_the_budget_hands_itself_to_a_queued_waiter() {
    let witnessed = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&witnessed);
    explore(
        "a_lock_refused_for_the_budget_hands_itself_to_a_queued_waiter",
        3,
        20,
        move || lock_refusal(true, true, Arc::clone(&seen)),
    );
    assert!(
        witnessed.load(Ordering::SeqCst) > 0,
        "the search never refused a lock while the waiter was queued"
    );
}

#[test]
fn a_lock_refused_for_the_budget_anywhere_in_the_waiters_attempt_loses_nothing() {
    explore(
        "a_lock_refused_for_the_budget_anywhere_in_the_waiters_attempt_loses_nothing",
        3,
        20,
        || lock_refusal(true, false, Arc::new(AtomicUsize::new(0))),
    );
}

/// The refusal without its give back: the waiter that queued behind the
/// permit the refused attempt took is never served, which loom reports as a
/// deadlock. Shared state lives in `std` `Arc`s (see the calibration of
/// `super::mpsc`).
#[test]
#[should_panic(expected = "deadlock")]
fn calibration_a_refused_lock_that_keeps_the_permit_strands_the_waiter() {
    explore(
        "calibration_a_refused_lock_that_keeps_the_permit_strands_the_waiter",
        3,
        1,
        || lock_refusal(false, true, Arc::new(AtomicUsize::new(0))),
    );
}

#[test]
fn a_send_refused_for_the_budget_hands_its_slot_to_a_queued_send() {
    let witnessed = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&witnessed);
    explore(
        "a_send_refused_for_the_budget_hands_its_slot_to_a_queued_send",
        3,
        20,
        move || {
            let sem = Arc::new(FairSemaphore::new(1));
            let waited = Arc::new(AtomicBool::new(false));
            let sender = {
                let (sem, waited) = (Arc::clone(&sem), Arc::clone(&waited));
                thread::spawn(move || {
                    let mut wait = FairWait::new();
                    let taken = block_on(poll_fn(|cx| {
                        let poll = wait.poll(&sem, 1, cx.waker());
                        if poll.is_pending() {
                            waited.store(true, Ordering::SeqCst);
                        }
                        poll
                    }));
                    assert!(taken.is_ok(), "the semaphore never closes here");
                    sem.release(1);
                })
            };
            let mut wait = FairWait::new();
            let refused = wait.poll_admitted(&sem, 1, Waker::noop(), || {
                if waited.load(Ordering::SeqCst) {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                false
            });
            match refused {
                // Refused, or queued behind the other send: a queued wait leaves the
                // line, passing on whatever was granted to it.
                core::task::Poll::Pending => wait.cancel(&sem, 1),
                // Queued and granted at once by the other send's release.
                core::task::Poll::Ready(taken) => {
                    assert!(taken.is_ok(), "the semaphore never closes here");
                    sem.release(1);
                }
            }
            sender.join().expect("sender");
            assert_eq!(sem.available(), 1, "a slot was lost or made up");
        },
    );
    assert!(
        witnessed.load(Ordering::SeqCst) > 0,
        "the search never refused a send while the other was queued"
    );
}
