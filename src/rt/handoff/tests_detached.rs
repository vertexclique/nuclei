//! The rest of a poll the monitor took, seen from inside the crate: a spawn with no worker
//! taking tasks goes to the lost shard, and a file write is counted on it
//! (proofs/tla/handoff/Detached.tla).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use crate::rt::shard::Shared;
use crate::runtime::{Builder, Runtime};

const SAFETY: Duration = Duration::from_secs(10);

fn on() -> Runtime {
    Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .build()
        .expect("the runtime starts")
}

/// Runs `rest` in the first poll of a task of worker 0, after a block the test ends once the
/// shard moved to another thread and `after_move` ran; answers what `rest` sent.
fn after_take<R>(
    rt: &Runtime,
    after_move: impl FnOnce(&Runtime),
    rest: impl FnOnce() -> R + Send + 'static,
) -> Receiver<R>
where
    R: Send + 'static,
{
    let open = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let (held, seen) = (Arc::clone(&open), Arc::clone(&started));
    let (out, answer) = channel();
    let handle = rt.handle();
    handle
        .spawn_on_worker(0, async move {
            seen.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + SAFETY;
            while !held.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let _ = out.send(rest());
        })
        .unwrap()
        .detach();
    let deadline = Instant::now() + SAFETY;
    while !started.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the blocking task never ran");
        std::thread::yield_now();
    }
    rt.block_on(handle.spawn_on_worker(0, async {}).unwrap());
    after_move(rt);
    open.store(true, Ordering::SeqCst);
    answer
}

#[test]
fn a_spawn_with_no_worker_taking_tasks_runs_on_the_lost_shards_new_thread() {
    let rt = on();
    let blocked = Arc::new(std::sync::OnceLock::new());
    let kept = Arc::clone(&blocked);
    let answer = after_take(
        &rt,
        |rt| rt.inner().workers.shards().for_each(|shard| shard.drain()),
        move || {
            let (ran, seen) = channel();
            crate::spawn(async move {
                let _ = ran.send(std::thread::current().id());
            })
            .detach();
            let _ = kept.set(std::thread::current().id());
            seen
        },
    );
    let seen = answer
        .recv_timeout(SAFETY)
        .expect("the rest of the poll ended");
    let ran = seen
        .recv_timeout(SAFETY)
        .expect("the spawned task never ran: it went to a shard nobody runs");
    assert_ne!(Some(&ran), blocked.get(), "it ran on the blocked thread");
}

#[test]
fn a_file_write_in_the_rest_of_a_taken_poll_is_counted_on_the_lost_shard() {
    let rt = on();
    let answer = after_take(
        &rt,
        |_| (),
        || {
            let write = crate::fs::call::WriteCall::here();
            let lost: Option<Arc<Shared>> =
                crate::rt::context::lost_shared(|shared| shared.map(Arc::clone));
            let counted = write.is_some();
            let while_it_lives = lost.as_ref().map(|shared| shared.settled());
            drop(write);
            (counted, while_it_lives, lost.map(|shared| shared.settled()))
        },
    );
    let (counted, while_it_lives, once_dropped) = answer
        .recv_timeout(SAFETY)
        .expect("the rest of the poll ended");
    assert!(counted, "the write was counted on no shard");
    assert_eq!(
        while_it_lives,
        Some(false),
        "the shard settled with a write out"
    );
    assert_eq!(once_dropped, Some(true), "the shard stayed unsettled");
}
