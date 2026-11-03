//! The paused clock across threads: work another thread posts to a parked
//! shard, and the jumps it must not hold up.

use core::future::Future;
use core::pin::Pin;
use core::task::Poll;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crate::runtime::{Builder, Runtime};
use crate::time;

/// How long a test waits for what must happen at once, before it fails.
const SAFETY: Duration = Duration::from_secs(20);

fn paused(workers: usize) -> Runtime {
    Builder::new()
        .worker_threads(workers)
        .pin_workers(false)
        .start_paused(true)
        .build()
        .unwrap()
}

/// Waits for `done`, looking again after a pause that doubles up to 5 ms,
/// and fails past [`SAFETY`].
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + SAFETY;
    let mut pause = Duration::from_micros(10);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "{what} never happened"
        );
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

/// `rt.block_on(future)` on a thread of its own, answered within [`SAFETY`].
fn block_on_within<F>(rt: &Runtime, future: F) -> Result<(), mpsc::RecvTimeoutError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let handle = rt.handle();
    let (done, ended) = mpsc::channel();
    std::thread::spawn(move || {
        handle.block_on(future);
        let _ = done.send(());
    });
    ended.recv_timeout(SAFETY)
}

#[test]
fn a_timer_removed_off_its_parked_shard_lets_the_clock_jump() {
    let rt = paused(1);
    let inner = Arc::clone(rt.inner());
    let start = inner.clock.now_nanos();
    let (out, armed) = mpsc::channel();
    rt.spawn(async move {
        let mut sleep = time::sleep(Duration::from_secs(3600));
        let pending =
            core::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut sleep).poll(cx).is_pending()))
                .await;
        out.send((pending, sleep)).unwrap();
    })
    .detach();
    let (pending, sleep) = armed.recv_timeout(SAFETY).unwrap();
    assert!(pending, "the sleep was armed on the worker");
    let worker = Arc::clone(inner.workers.shards().next().unwrap());
    // The clock jumps to the hour, the worker fires the timer and parks
    // again with none left: from then on nothing wakes it.
    eventually("the worker parked for good past the hour", || {
        inner.clock.now_nanos() >= start + 3_600_000_000_000
            && worker.timer_at() == u64::MAX
            && worker.idle()
    });
    // Dropped outside every shard: its removal is posted to the worker.
    drop(sleep);
    assert!(
        block_on_within(&rt, async { time::sleep(Duration::from_secs(1)).await }).is_ok(),
        "the clock never jumped to the next sleep: the worker sleeps on, holding the removal"
    );
}

/// A waker that notes the clock at the moment it is woken.
struct Stamp {
    inner: Arc<crate::rt::runtime::Inner>,
    at: std::sync::Mutex<Option<std::time::Instant>>,
}

impl std::task::Wake for Stamp {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let now = self.inner.clock.now();
        *self.at.lock().unwrap() = Some(now);
    }
}

/// A sleep armed from a plain thread on a paused clock, found pending by its
/// poll, is woken with the clock at its deadline: a worker parked with a
/// later timer does not jump past it between the poll's look at the clock
/// and its post (`proofs/tla/clock/ForeignArm.tla`).
#[test]
fn a_foreign_arm_is_never_jumped_past() {
    let rt = paused(1);
    rt.spawn(async {
        loop {
            time::sleep(Duration::from_secs(10 * 3600)).await;
        }
    })
    .detach();
    let _inside = rt.enter();
    let stamp = Arc::new(Stamp {
        inner: Arc::clone(rt.inner()),
        at: std::sync::Mutex::new(None),
    });
    let waker = core::task::Waker::from(Arc::clone(&stamp));
    let mut cx = core::task::Context::from_waker(&waker);
    let mut armed = 0;
    for round in 0..400 {
        let mut sleep = time::sleep(Duration::from_millis(1));
        let deadline = sleep.deadline();
        *stamp.at.lock().unwrap() = None;
        if Pin::new(&mut sleep).poll(&mut cx).is_ready() {
            // Its deadline passed before the poll: a jump of the worker alone.
            continue;
        }
        armed += 1;
        let mut woken_at = None;
        eventually(&format!("the foreign sleep woken in round {round}"), || {
            woken_at = *stamp.at.lock().unwrap();
            woken_at.is_some()
        });
        let at = time::Instant::from_std(woken_at.unwrap());
        assert!(
            at <= deadline + Duration::from_millis(1),
            "round {round}: the clock jumped past the foreign sleep"
        );
        assert!(Pin::new(&mut sleep).poll(&mut cx).is_ready());
    }
    assert!(armed > 0, "every sleep ended before its first poll");
}
