//! A runtime's `block_on` nested on a shard another runtime serves: each
//! keeps its timers in wheels read by its own clock
//! (`proofs/tla/clock/OneWheel.tla`).

use core::future::Future;
use core::pin::Pin;
use core::task::Poll;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::time::Duration;

use crate::runtime::{Builder, Runtime};
use crate::time;

fn runtime(workers: usize) -> Runtime {
    Builder::new()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .unwrap()
}

/// Polls `f` once, from inside the loop that runs the caller.
async fn poll_once<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    core::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut *f).poll(cx))).await
}

/// What a sleep of the serving runtime and a sleep of the nested one do when
/// the nested runtime pauses its clock and moves it by 5 s.
async fn serving_and_nested(nested: &Runtime) {
    let mut serving = time::sleep(Duration::from_secs(3));
    assert!(poll_once(&mut serving).await.is_pending());
    let began = time::Instant::now();
    nested.block_on(async {
        time::pause();
        let t0 = time::Instant::now();
        let mut inside = time::sleep(Duration::from_secs(5));
        assert!(poll_once(&mut inside).await.is_pending());
        time::advance(Duration::from_secs(5)).await;
        inside.await;
        assert!(time::Instant::now() - t0 >= Duration::from_secs(5));
    });
    // The serving runtime's clock did not move, and nor did its sleep end.
    assert!(time::Instant::now() - began < Duration::from_secs(2));
    assert!(
        poll_once(&mut serving).await.is_pending(),
        "the serving runtime's sleep ended by the nested runtime's clock"
    );
}

#[test]
fn a_nested_runtime_keeps_its_clock_and_wheels_on_an_own_shard() {
    let serving = runtime(0);
    let nested = runtime(0);
    serving.block_on(serving_and_nested(&nested));
}

/// A worker's shard is watched, its polls open to the handoff: the nested
/// future shares the worker's wheels, so its sleeps fire by the worker's
/// runtime's clock, and the worker's own sleep is not disturbed.
#[test]
fn a_nested_runtime_on_a_worker_shard_shares_the_workers_wheels() {
    let serving = runtime(1);
    let nested = std::sync::Arc::new(runtime(0));
    let (done, ended) = mpsc::channel();
    let other = std::sync::Arc::clone(&nested);
    serving
        .spawn(async move {
            let mut own = time::sleep(Duration::from_secs(3600));
            assert!(poll_once(&mut own).await.is_pending());
            other.block_on(async {
                time::sleep(Duration::from_millis(20)).await;
            });
            assert!(poll_once(&mut own).await.is_pending());
            let _ = done.send(());
        })
        .detach();
    ended
        .recv_timeout(Duration::from_secs(20))
        .expect("the worker never finished the nested block_on");
}

/// A nested future of another runtime that blocks its worker past the
/// handoff takes the shard to a new runner: it finishes, and the worker's own
/// timers still fire, with no wheel swapped back into a shard another thread
/// runs.
#[test]
fn a_nested_runtime_blocking_a_worker_past_the_handoff_completes() {
    let serving = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(5)))
        .build()
        .unwrap();
    let nested = std::sync::Arc::new(runtime(0));
    let (done, ended) = mpsc::channel();
    let other = std::sync::Arc::clone(&nested);
    let ticker = serving.spawn(async {
        time::sleep(Duration::from_millis(30)).await;
    });
    serving
        .spawn(async move {
            other.block_on(async {
                // Blocks the thread in one poll, past the handoff.
                std::thread::sleep(Duration::from_millis(300));
                time::sleep(Duration::from_millis(10)).await;
            });
            let _ = done.send(());
        })
        .detach();
    ended
        .recv_timeout(Duration::from_secs(20))
        .expect("the nested block_on never finished after the handoff");
    serving.block_on(ticker);
}

#[test]
fn an_unwind_out_of_the_nested_future_leaves_the_wheels_in_place() {
    let serving = runtime(0);
    let nested = runtime(0);
    serving.block_on(async {
        let mut sleep = time::sleep(Duration::from_secs(3600));
        assert!(poll_once(&mut sleep).await.is_pending());
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            nested.block_on(async {
                let mut inside = time::sleep(Duration::from_secs(3600));
                assert!(poll_once(&mut inside).await.is_pending());
                panic!("the nested future failed");
            });
        }));
        assert!(unwound.is_err());
        crate::rt::context::current(|shard| {
            let shard = shard.expect("inside the serving runtime's loop");
            assert_eq!(
                shard.wheel().borrow().armed(),
                1,
                "the serving wheel is back"
            );
            assert!(shard.side.borrow().is_empty(), "the nested wheels went");
            assert!(!shard.foreign_poll.get());
            assert_eq!(shard.wheel_of(true).borrow().armed(), 0);
        });
        assert!(poll_once(&mut sleep).await.is_pending());
    });
}

#[test]
fn a_nested_runtimes_running_sleep_fires_by_its_own_clock_between_polls() {
    let serving = runtime(0);
    let nested = runtime(0);
    serving.block_on(async {
        nested.block_on(async {
            // Armed in the nested wheels, fired by the loop between the polls.
            time::sleep(Duration::from_millis(20)).await;
            time::sleep_until(time::Instant::now() + Duration::from_millis(5)).await;
        });
    });
}
