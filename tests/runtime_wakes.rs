//! Wakes between threads are never lost: tasks on workers, plain threads and
//! a future in `block_on` keep waking each other's threads, in rounds of
//! every length (one message per task, its tasks spawned and gone at once,
//! up to thousands), each round under a deadline. A shard used to park with
//! tasks still in its inbox: its last look before sleeping read the queue's
//! head alone, which is the stub while the shard's own pop puts the stub back
//! behind the last task with a push that swapped in just before it still
//! linking. About one round in 30,000 of the channel's slept through so.
//! `NUCLEI_WAKE_SECS` sets how long each test runs its rounds (1 second
//! unless set).
#![cfg(not(target_arch = "wasm32"))]

use std::future::Future;
use std::time::{Duration, Instant};

use nuclei::Builder;
use nuclei::sync::mpsc;

/// A round takes microseconds; past this it slept through a wake.
const ROUND_DEADLINE: Duration = Duration::from_secs(10);

fn duration() -> Duration {
    let secs = std::env::var("NUCLEI_WAKE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    Duration::from_secs(secs)
}

/// Runs `round(per)` in `block_on` on a runtime of `workers` until the test's
/// time is up, `per` going from 1 to 4096 and round again; a round past its
/// deadline fails the test with what every shard showed then.
fn rounds<Fut: Future<Output = u64>>(workers: usize, mut round: impl FnMut(u64) -> Fut) {
    let rt = Builder::new()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .unwrap();
    let end = Instant::now() + duration();
    let mut n = 0u64;
    while Instant::now() < end {
        let per = 1u64 << (n % 13);
        n += 1;
        let ended = rt.block_on(nuclei::time::timeout(ROUND_DEADLINE, round(per)));
        match ended {
            Ok(got) => assert_eq!(got, 8 * per, "round {n}: a message was lost"),
            Err(_) => panic!(
                "round {n} ({per} messages a sender) never ended, a wake lost: {:#?}",
                nuclei::stats()
            ),
        }
    }
}

/// Eight producer tasks on the workers and the consumer in `block_on`, on a
/// bounded channel: the consumer's thread wakes the producers' worker for
/// room, and the worker wakes the consumer's thread for each message.
async fn bounded_round(per: u64) -> u64 {
    let (tx, mut rx) = mpsc::channel::<u64>(1024);
    for _ in 0..8 {
        let tx = tx.clone();
        nuclei::runtime::spawn(async move {
            for i in 0..per {
                tx.send(i).await.unwrap();
            }
        })
        .detach();
    }
    drop(tx);
    let mut got = 0;
    while rx.recv().await.is_some() {
        got += 1;
    }
    got
}

#[test]
fn producers_on_workers_and_a_consumer_in_block_on_never_lose_a_wake() {
    rounds(4, bounded_round);
}

#[test]
fn producers_on_one_worker_and_a_consumer_in_block_on_never_lose_a_wake() {
    rounds(1, bounded_round);
}

/// Eight consumer tasks on one worker, each fed by a plain thread of its
/// own: eight threads push woken tasks into the worker's inbox at once, while
/// it takes them. The round awaits the consumers from `block_on`.
async fn fan_in_round(per: u64) -> u64 {
    let mut feeders = Vec::new();
    let mut consumers = Vec::new();
    for _ in 0..8 {
        let (tx, mut rx) = mpsc::unbounded_channel::<u64>();
        consumers.push(nuclei::runtime::spawn(async move {
            let mut got = 0;
            while rx.recv().await.is_some() {
                got += 1;
            }
            got
        }));
        feeders.push(std::thread::spawn(move || {
            for i in 0..per {
                tx.send(i).unwrap();
            }
        }));
    }
    let mut got = 0;
    for consumer in consumers {
        got += consumer.await;
    }
    for feeder in feeders {
        feeder.join().unwrap();
    }
    got
}

#[test]
fn tasks_woken_from_many_threads_at_once_never_lose_a_wake() {
    rounds(4, fan_in_round);
}
