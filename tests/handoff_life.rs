//! A thread whose shard was taken while its poll blocked, through the public surface: once that
//! poll returns, the thread is a runtime thread like any other again. Its bound tasks finish on
//! it, and a runtime dropped meanwhile waits for it to exit before the drop returns
//! (proofs/tla/handoff/TakeRef.tla, `LifeMeansBlocked`).
//!
//! Nothing is timed against a bound: the blocked poll holds its thread until the test saw the
//! stats count the handoff, and each wait has a deadline far past anything a working runtime
//! needs.

use core::cell::RefCell;
use core::task::Poll;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nuclei::runtime::Builder;

/// How long a wait holds at most: far past anything a working handoff needs.
const SAFETY: Duration = Duration::from_secs(10);

/// Waits for `done`, with a deadline and a backoff that starts at once.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + SAFETY;
    let mut pause = Duration::from_micros(50);
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

/// Sets its flag as it drops, a while after its drop began: dropped as its thread exits, it has
/// not set it yet when a runtime drop that did not wait for that thread returns.
struct SlowDrop(Arc<AtomicBool>);

impl Drop for SlowDrop {
    fn drop(&mut self) {
        std::thread::sleep(Duration::from_millis(200));
        self.0.store(true, Ordering::SeqCst);
    }
}

thread_local! {
    /// Dropped as the thread that set it exits.
    static AT_EXIT: RefCell<Option<SlowDrop>> = const { RefCell::new(None) };
}

#[test]
fn a_drop_waits_for_a_thread_back_from_its_blocked_poll_running_its_bound_tasks() {
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .build()
        .expect("the runtime starts");
    let open = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));
    let (blocking_tx, blocking_rx) = std::sync::mpsc::channel::<()>();
    let (bound_tx, bound_rx) = std::sync::mpsc::channel::<core::task::Waker>();
    let gate = Arc::clone(&open);
    let flag = Arc::clone(&exited);
    rt.spawn(async move {
        let (wake, woken) = nuclei::sync::oneshot::channel::<()>();
        // Bound to this thread. Woken only once the blocked poll below returns, it runs then on
        // the thread's own shard, the worker's having gone to another thread, and stays live:
        // the test holds its waker.
        nuclei::spawn_local(async move {
            let _ = woken.await;
            AT_EXIT.with(|at_exit| *at_exit.borrow_mut() = Some(SlowDrop(flag)));
            core::future::poll_fn(|cx| {
                bound_tx.send(cx.waker().clone()).unwrap();
                Poll::<()>::Pending
            })
            .await;
        })
        .detach();
        blocking_tx.send(()).unwrap();
        // One poll that blocks until the test saw its shard handed off.
        eventually("the test's word to go on", || gate.load(Ordering::SeqCst));
        let _ = wake.send(());
    })
    .detach();
    blocking_rx
        .recv_timeout(SAFETY)
        .expect("the blocking task never ran");
    eventually("the handoff of the blocked poll's shard", || {
        rt.stats().shards.iter().any(|shard| shard.handoffs >= 1)
    });
    open.store(true, Ordering::SeqCst);
    let bound = bound_rx
        .recv_timeout(SAFETY)
        .expect("the bound task never ran on its thread after the poll returned");
    drop(rt);
    assert!(
        exited.load(Ordering::SeqCst),
        "the drop returned before the thread back from its blocked poll exited"
    );
    // The bound task's future is not `Send`: its last waker is not dropped off its thread.
    core::mem::forget(bound);
}
