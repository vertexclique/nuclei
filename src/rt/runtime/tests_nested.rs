//! A task polled inside another runtime's nested `block_on` sees its own
//! runtime, and the wheels of a future being polled go aside for a frame
//! nested in its poll (`proofs/tla/nested/Nested.tla`).

use core::future::poll_fn;
use core::task::Poll;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::{Builder, Runtime, RuntimeHandle, RuntimeId};
use crate::runtime::worker_index;

const LIMIT: Duration = Duration::from_secs(10);

fn runtime(workers: usize) -> Runtime {
    Builder::new()
        .worker_threads(workers)
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .unwrap()
}

fn here() -> RuntimeId {
    RuntimeHandle::try_current().unwrap().id()
}

/// What a probe saw: its runtime, its worker, the runtime of a task it spawned.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    runtime: RuntimeId,
    worker: Option<usize>,
    child: RuntimeId,
}

/// A task that waits for `go`, then records what it sees and what its child does.
async fn probe(go: Arc<AtomicBool>, tx: mpsc::Sender<Seen>) {
    wait(|| go.load(Ordering::SeqCst)).await;
    let child = crate::spawn(async { here() }).await;
    let _ = tx.send(Seen {
        runtime: here(),
        worker: worker_index(),
        child,
    });
}

/// Pending (waking itself, so the loop it sits in goes on) until `done`, or `LIMIT`.
async fn wait(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + LIMIT;
    poll_fn(|cx| {
        if done() || Instant::now() >= deadline {
            return Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
}

/// Sets `go`, then waits (inside the caller's nested loop) for the probe's report.
async fn release(go: &AtomicBool, rx: &mpsc::Receiver<Seen>) -> Seen {
    go.store(true, Ordering::SeqCst);
    let mut seen = None;
    wait(|| {
        seen = rx.try_recv().ok();
        seen.is_some()
    })
    .await;
    seen.expect("the probe reported within the deadline")
}

struct Rig {
    a: Runtime,
    go: Arc<AtomicBool>,
    rx: mpsc::Receiver<Seen>,
}

impl Rig {
    /// Runtime A with a probe task on its one worker.
    fn new() -> Self {
        let a = runtime(1);
        let go = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        a.spawn(probe(Arc::clone(&go), tx)).detach();
        Self { a, go, rx }
    }
}

fn expect_a(a: RuntimeId, seen: &Seen) {
    assert_eq!(
        *seen,
        Seen {
            runtime: a,
            worker: Some(0),
            child: a,
        }
    );
}

#[test]
fn a_task_polled_in_another_runtimes_block_on_sees_its_own() {
    let rig = Rig::new();
    let b = runtime(1);
    let (hb, go, rx) = (b.handle(), Arc::clone(&rig.go), rig.rx);
    let (seen, inside) = rig.a.block_on(rig.a.spawn(async move {
        hb.block_on(async {
            let seen = release(&go, &rx).await;
            (seen, (here(), worker_index()))
        })
    }));
    assert_eq!(seen.runtime, rig.a.id());
    assert_eq!(seen.worker, Some(0));
    assert_eq!(seen.child, rig.a.id());
    assert_eq!(inside, (b.id(), None));
}

#[test]
fn an_enter_guard_of_another_runtime_does_not_move_the_loop() {
    let rig = Rig::new();
    let b = runtime(1);
    let (hb, go, rx) = (b.handle(), Arc::clone(&rig.go), rig.rx);
    let seen = rig.a.block_on(rig.a.spawn(async move {
        let _guard = hb.enter();
        crate::block_on(async { release(&go, &rx).await })
    }));
    expect_a(rig.a.id(), &seen);
}

#[test]
fn a_third_runtime_nested_in_a_foreign_future_does_not_move_the_loop() {
    let rig = Rig::new();
    let (b, c) = (runtime(1), runtime(1));
    let (hb, hc, go, rx) = (b.handle(), c.handle(), Arc::clone(&rig.go), rig.rx);
    let seen = rig.a.block_on(rig.a.spawn(async move {
        hb.block_on(async { hc.block_on(async { release(&go, &rx).await }) })
    }));
    expect_a(rig.a.id(), &seen);
}

#[test]
fn a_panic_out_of_a_foreign_future_leaves_the_shard_on_its_runtime() {
    let rig = Rig::new();
    let b = runtime(1);
    let (hb, go, rx) = (b.handle(), Arc::clone(&rig.go), rig.rx);
    let (seen, after, spawned) = rig.a.block_on(rig.a.spawn(async move {
        let mut seen = None;
        let caught = catch_unwind(AssertUnwindSafe(|| {
            hb.block_on(async {
                seen = Some(release(&go, &rx).await);
                panic!("out of the foreign future");
            })
        }));
        assert!(caught.is_err());
        (seen, here(), crate::spawn(async { here() }).await)
    }));
    expect_a(rig.a.id(), &seen.unwrap());
    assert_eq!((after, spawned), (rig.a.id(), rig.a.id()));
}

#[test]
fn a_frame_nested_in_a_foreign_poll_fires_the_shards_own_timers() {
    let (a, b, c) = (runtime(0), runtime(0), runtime(0));
    let (hb, hc) = (b.handle(), c.handle());
    let fired = Arc::new(AtomicBool::new(false));
    let seen = a.block_on(async {
        let flag = Arc::clone(&fired);
        let _sleeper = crate::spawn_local(async move {
            crate::time::sleep(Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });
        for _ in 0..3 {
            crate::task::yield_now().await;
        }
        let flag = Arc::clone(&fired);
        hb.block_on(async move {
            hc.block_on(async move {
                wait(|| flag.load(Ordering::SeqCst)).await;
                flag.load(Ordering::SeqCst)
            })
        })
    });
    assert!(
        seen,
        "the outer frame's timer fires while the inner frame waits"
    );
}

/// A task that a runtime with no worker spawns inside its `block_on` nested on another
/// runtime's worker is a visitor: it, and what it spawns, sees its own runtime (`TaskSeesOwn`).
#[test]
fn a_visitor_of_a_runtime_with_no_worker_sees_its_own_runtime() {
    let l = runtime(1);
    let x = Arc::new(runtime(0));
    let x_id = x.handle().id();
    let nested = Arc::clone(&x);
    let seen = l.block_on(l.spawn(async move {
        let seen = nested.block_on(async {
            crate::spawn(async { (here(), crate::spawn(async { here() }).await) }).await
        });
        drop(nested);
        (here(), seen)
    }));
    assert_eq!(seen.0, l.handle().id(), "the worker's own task");
    assert_eq!(seen.1, (x_id, x_id), "the visitor and its child");
}

/// On a thread's own shard the outermost `block_on` marks the runtime its loop serves, so a task
/// spawned under an enter guard of a runtime with no worker is that runtime's visitor.
#[test]
fn a_task_spawned_under_an_enter_guard_on_an_own_shard_sees_the_guards_runtime() {
    let l = runtime(0);
    let x = runtime(0);
    let x_id = x.handle().id();
    let seen = l.block_on(async {
        let task = {
            let _x = x.enter();
            crate::spawn(async { here() })
        };
        (task.await, here())
    });
    assert_eq!(seen, (x_id, l.handle().id()));
}
