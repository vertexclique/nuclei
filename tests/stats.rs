//! `nuclei::stats()`: the shards it lists and when, the blocking pools'
//! threads, and how its error counts read. Each test holds `ONE`, so the
//! shards one makes never show in another's counts, and runs `block_on`
//! only on threads it joins: the shard of the test's own thread would go
//! only as that thread exits, after the lock is let go.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::fs::File;
use std::io::ErrorKind;
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use nuclei::stats::{ErrorCounts, Stats};
use nuclei::{Builder, Handle};

static ONE: Mutex<()> = Mutex::new(());

fn one() -> MutexGuard<'static, ()> {
    ONE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How many shards hold each worker place, and how many are threads' own.
fn places(stats: &Stats) -> (HashMap<usize, usize>, usize) {
    let mut workers = HashMap::new();
    let mut own = 0;
    for shard in &stats.shards {
        match shard.worker {
            Some(place) => *workers.entry(place).or_default() += 1,
            None => own += 1,
        }
    }
    (workers, own)
}

/// Waits until `cond` holds of a fresh snapshot, for at most ten seconds.
fn until(what: &str, mut cond: impl FnMut(&Stats) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(50);
    loop {
        let stats = nuclei::stats();
        if cond(&stats) {
            return;
        }
        assert!(Instant::now() < deadline, "never {what}: {stats:?}");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

#[test]
fn a_runtimes_workers_are_listed_by_place_until_it_stops() {
    let _one = one();
    let (before, _) = places(&nuclei::stats());
    let at = |workers: &HashMap<usize, usize>, place| workers.get(&place).copied().unwrap_or(0);
    let rt = Builder::new()
        .worker_threads(2)
        .pin_workers(false)
        .build()
        .unwrap();
    until("both workers listed", |stats| {
        let (now, _) = places(stats);
        at(&now, 0) == at(&before, 0) + 1 && at(&now, 1) == at(&before, 1) + 1
    });
    let stats = nuclei::stats();
    let first_own = stats.shards.iter().position(|s| s.worker.is_none());
    let last_worker = stats.shards.iter().rposition(|s| s.worker.is_some());
    if let (Some(own), Some(worker)) = (first_own, last_worker) {
        assert!(worker < own, "the workers come first: {stats:?}");
    }
    drop(rt);
    until("the workers gone", |stats| {
        let (now, _) = places(stats);
        at(&now, 0) == at(&before, 0) && at(&now, 1) == at(&before, 1)
    });
}

#[test]
fn a_threads_shard_is_listed_while_the_thread_lives() {
    let _one = one();
    let (_, before) = places(&nuclei::stats());
    let inside = std::thread::spawn(|| nuclei::block_on(async { places(&nuclei::stats()).1 }))
        .join()
        .unwrap();
    assert_eq!(inside, before + 1);
    assert_eq!(places(&nuclei::stats()).1, before, "gone with its thread");
}

#[test]
fn blocking_threads_count_the_pools_threads() {
    let _one = one();
    let rt = Builder::new().worker_threads(1).build().unwrap();
    let (release, wait) = mpsc::channel::<()>();
    let (started, running) = mpsc::channel::<()>();
    let job = rt.spawn_blocking(move || {
        started.send(()).unwrap();
        wait.recv().unwrap();
    });
    running.recv().unwrap();
    assert!(nuclei::stats().blocking_threads >= 1);
    release.send(()).unwrap();
    // On a thread of its own, joined: its shard is gone before the next
    // test counts shards (a test's own thread outlives the test).
    std::thread::spawn(move || rt.block_on(job)).join().unwrap();
}

#[test]
fn errors_read_by_kind() {
    let _one = one();
    let none = ErrorCounts::default();
    assert_eq!(none.get(ErrorKind::NotFound), 0);
    assert_eq!(none.iter().count(), 0);
    assert_eq!(
        (none.refused_registrations, none.accepts_out_of_descriptors),
        (0, 0)
    );

    let (before, after, err) = std::thread::spawn(|| {
        nuclei::block_on(async {
            let not_found = |stats: Stats| {
                stats
                    .shards
                    .iter()
                    .filter(|s| s.worker.is_none())
                    .map(|s| s.errors.get(ErrorKind::NotFound))
                    .sum::<u64>()
            };
            let before = not_found(nuclei::stats());
            let err = Handle::<File>::open("/nonexistent/nuclei/stats")
                .await
                .unwrap_err();
            (before, not_found(nuclei::stats()), err)
        })
    })
    .join()
    .unwrap();
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert_eq!(after, before + 1);
}

#[test]
fn an_error_lists_with_its_kind() {
    let _one = one();
    let errors = std::thread::spawn(|| {
        nuclei::block_on(async {
            Handle::<File>::open("/nonexistent/nuclei/listed")
                .await
                .unwrap_err();
            let stats = nuclei::stats();
            let mine = stats.shards.iter().find(|s| s.worker.is_none()).cloned();
            mine.expect("this thread's shard").errors
        })
    })
    .join()
    .unwrap();
    assert_eq!(
        errors.iter().collect::<Vec<_>>(),
        [(ErrorKind::NotFound, 1)]
    );
    assert!(format!("{errors:?}").contains("NotFound"), "{errors:?}");
}

#[test]
fn a_workers_sleeps_and_busy_time_are_counted() {
    let _one = one();
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .unwrap();
    // The worker runs a task, then has nothing to do: it parks.
    std::thread::spawn({
        let handle = rt.handle();
        move || {
            handle.block_on(async {
                nuclei::spawn(async {
                    nuclei::time::sleep(Duration::from_millis(30)).await;
                })
                .await;
            })
        }
    })
    .join()
    .unwrap();
    until("the worker's parks published", |stats| {
        stats.shards.iter().any(|shard| {
            shard.worker == Some(0) && shard.parks >= 1 && shard.parked > Duration::ZERO
        })
    });
    let stats = nuclei::stats();
    let worker = stats
        .shards
        .iter()
        .find(|shard| shard.worker == Some(0) && shard.parks >= 1)
        .unwrap();
    assert!(worker.busy + worker.parked > Duration::ZERO);
    assert!(
        worker.queue_depth <= 1,
        "an idle worker's queue is empty: {worker:?}"
    );
    drop(rt);
}
