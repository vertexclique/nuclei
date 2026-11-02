//! A stress of the shard handoff and its watch, through the public surface: in every round,
//! tasks block their thread past the watch while others spawn and join, wake one another, send
//! and receive through channels, run a nested `block_on` that blocks, and hold a `shard_local!`
//! value across a nested `block_on` that blocks under its `with`. Under the default watch, the
//! first round blocks nowhere, so its calls from tasks' code take the leaf push and the bracket
//! on shards nobody takes.
//!
//! Every check is on what the tasks computed, never on timing: each task's result, every message
//! received once and in order, each nested `block_on`'s output, and a shard-local value its
//! `with` holds unchanged. A watchdog thread ends the process, naming the round and the step it
//! stopped in, once no step ends within [`STALL`]: a hang fails in seconds, not at the job's
//! timeout.

use std::cell::Cell;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::Duration;

use nuclei::runtime::{Builder, RuntimeHandle};
use nuclei::sync::{mpsc, oneshot};

nuclei::shard_local! { static TALLY: Cell<u64> = Cell::new(0) }

/// Rounds of each runtime.
const ROUNDS: usize = 20;
/// The watch: a poll past it is handed off.
const WATCH: Duration = Duration::from_millis(5);
/// How long a blocking task holds its thread: past the watch.
const BLOCK: Duration = Duration::from_millis(12);
/// How long a step may take before the watchdog calls it a hang.
const STALL: Duration = Duration::from_secs(20);

/// The round and the step the stress is at, told to a watchdog thread that ends the process
/// once none comes within [`STALL`], and that ends itself as this drops.
struct Progress(Sender<(usize, usize)>);

impl Progress {
    fn watched(name: &'static str) -> Self {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut at = (0, 0);
            loop {
                match rx.recv_timeout(STALL) {
                    Ok(next) => at = next,
                    Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {
                        eprintln!(
                            "{name}: hung in round {} at step {} for {STALL:?}",
                            at.0, at.1
                        );
                        std::process::exit(1);
                    }
                }
            }
        });
        Self(tx)
    }

    fn at(&self, round: usize, step: usize) {
        let _ = self.0.send((round, step));
    }
}

fn runtime(workers: usize) -> Builder {
    Builder::new().worker_threads(workers).pin_workers(false)
}

/// Holds the thread past the watch, in a round that blocks.
fn hold(blocking: bool) {
    if blocking {
        std::thread::sleep(BLOCK);
    }
}

/// One round on `h`'s runtime, with `workers` workers, `blocking` its tasks past the watch or
/// not at all: every task's result, summed in the order the round awaits them, against what the
/// round expects.
async fn round(h: &RuntimeHandle, workers: usize, blocking: bool, progress: &Progress, r: usize) {
    progress.at(r, 1);
    // Tasks that block their thread past the watch, on every worker.
    let blocked_workers = if blocking { workers } else { 0 };
    let blockers: Vec<_> = (0..blocked_workers)
        .map(|k| {
            h.spawn_on_worker(k, async move {
                std::thread::sleep(BLOCK);
                k as u64
            })
            .expect("a worker takes the task")
        })
        .collect();

    // A chain of spawns, each joined.
    let chain = h.spawn(async {
        let mut sum = 0u64;
        for i in 0..8u64 {
            sum += nuclei::spawn(async move { i }).await;
        }
        sum
    });

    // Oneshot rounds: each answer sent by a task of its own.
    let oneshots = h.spawn(async {
        let mut sum = 0u64;
        for i in 0..8u64 {
            let (tx, rx) = oneshot::channel::<u64>();
            nuclei::spawn(async move {
                let _ = tx.send(i * i);
            })
            .detach();
            sum += rx.await.expect("the answer comes");
        }
        sum
    });

    // A ping-pong through bounded channels, its two ends on different workers when there are
    // two: every message answered once, in order.
    let (to_pong, mut at_pong) = mpsc::channel::<u64>(1);
    let (to_ping, mut at_ping) = mpsc::channel::<u64>(1);
    let pong = h
        .spawn_on_worker(workers - 1, async move {
            let mut seen = 0u64;
            while let Some(n) = at_pong.recv().await {
                assert_eq!(n, seen, "a message out of order");
                seen += 1;
                if to_ping.send(n + 100).await.is_err() {
                    break;
                }
            }
            seen
        })
        .expect("a worker takes the task");
    let ping = h
        .spawn_on_worker(0, async move {
            let mut sum = 0u64;
            for n in 0..16u64 {
                to_pong.send(n).await.expect("the pong is there");
                let back = at_ping.recv().await.expect("the pong answers");
                assert_eq!(back, n + 100, "an answer to another message");
                sum += back;
            }
            sum
        })
        .expect("a worker takes the task");

    // Timers and yields: wakes of the task by its shard.
    let ticks = h.spawn(async {
        for _ in 0..4 {
            nuclei::time::sleep(Duration::from_millis(1)).await;
            nuclei::task::yield_now().await;
        }
        4u64
    });

    // A nested block_on that waits on a timer and a channel, then blocks past the watch.
    let (to_nested, at_nested) = oneshot::channel::<u64>();
    let feeder = h.spawn(async move {
        nuclei::time::sleep(Duration::from_millis(1)).await;
        let _ = to_nested.send(21);
    });
    let nested = h.spawn(async move {
        nuclei::block_on(async move {
            nuclei::time::sleep(Duration::from_millis(1)).await;
            let got = at_nested.await.expect("the feeder sends");
            hold(blocking);
            got * 2
        })
    });

    // A shard-local value held across a nested block_on that blocks: under its `with` the
    // shard stays with the thread, so the value is the one the task set. Without a `with`, the
    // same block lets the shard go; the value is reached again from the task's next poll.
    let held = h.spawn(async move {
        TALLY.with(|t| {
            let mine = t.get() + 1;
            t.set(mine);
            let got = nuclei::block_on(async move {
                hold(blocking);
                1u64
            });
            (t.get() == mine, got)
        })
    });
    let free = h.spawn(async move {
        TALLY.with(|t| t.set(t.get() + 1));
        let got = nuclei::block_on(async move {
            hold(blocking);
            1u64
        });
        // The block may have let the shard go, leaving this poll on a thread that runs none:
        // the task's next poll is on its shard again, wherever that runs.
        nuclei::task::yield_now().await;
        TALLY.with(|t| t.set(t.get() + 1));
        got
    });

    progress.at(r, 2);
    let mut blocked = 0u64;
    for b in blockers {
        blocked += b.await;
    }
    assert_eq!(
        blocked,
        (0..blocked_workers as u64).sum::<u64>(),
        "a blocker's result"
    );
    progress.at(r, 3);
    assert_eq!(chain.await, 28, "the spawn chain's sum");
    progress.at(r, 4);
    assert_eq!(oneshots.await, 140, "the oneshot answers' sum");
    progress.at(r, 5);
    assert_eq!(
        ping.await,
        (0..16u64).map(|n| n + 100).sum::<u64>(),
        "the answers' sum"
    );
    progress.at(r, 6);
    assert_eq!(pong.await, 16, "the messages the pong saw");
    progress.at(r, 7);
    assert_eq!(ticks.await, 4);
    progress.at(r, 8);
    feeder.await;
    assert_eq!(nested.await, 42, "the nested block_on's output");
    progress.at(r, 9);
    assert_eq!(
        held.await,
        (true, 1),
        "the shard-local value changed under its with"
    );
    progress.at(r, 10);
    assert_eq!(free.await, 1);
}

/// [`ROUNDS`] rounds on a runtime `builder` makes with `workers` workers, every one blocking
/// but the first when `quiet_first`.
fn stress(name: &'static str, builder: Builder, workers: usize, quiet_first: bool) {
    let progress = Progress::watched(name);
    let rt = builder.build().expect("the runtime starts");
    let h = rt.handle();
    for r in 0..ROUNDS {
        let blocking = !(quiet_first && r == 0);
        rt.block_on(round(&h, workers, blocking, &progress, r));
    }
    progress.at(ROUNDS, 0);
    drop(rt);
}

#[test]
fn one_worker_keeps_every_task_and_message_through_handoffs() {
    stress(
        "one worker",
        runtime(1).handoff_after(Some(WATCH)),
        1,
        false,
    );
}

#[test]
fn two_workers_keep_every_task_and_message_through_handoffs() {
    stress(
        "two workers",
        runtime(2).handoff_after(Some(WATCH)),
        2,
        false,
    );
}

#[test]
fn the_default_watch_keeps_every_task_and_message_with_and_without_blocks() {
    stress("the default watch", runtime(2), 2, true);
}
