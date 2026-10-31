//! Per-runtime stats: `Runtime::stats` and `RuntimeHandle::stats` list that
//! runtime's workers by place, `nuclei::stats` lists every runtime's, the
//! blocking queue depth, budget yields, and poll-time histograms. Counts are
//! published as a shard looks at its driver, so they are read with a deadline.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nuclei::runtime::Runtime;
use nuclei::stats::{Histogram, Stats};

const SAFETY: Duration = Duration::from_secs(10);

fn runtime(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

fn recording(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .record_latencies(true)
        .build()
        .expect("the runtime starts")
}

#[derive(Default)]
struct Gate {
    open: AtomicBool,
    gave_up: AtomicBool,
}

impl Gate {
    fn hold(&self) {
        let deadline = Instant::now() + SAFETY;
        while !self.open.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                self.gave_up.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn release_after(&self, what: &str) {
        assert!(
            !self.gave_up.load(Ordering::SeqCst),
            "{what} waited out the block"
        );
        self.open.store(true, Ordering::SeqCst);
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + SAFETY;
    let mut pause = Duration::from_micros(50);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

fn assert_its_workers(stats: &Stats, rt: &Runtime, workers: usize) {
    assert_eq!(stats.shards.len(), workers);
    for (place, shard) in stats.shards.iter().enumerate() {
        assert_eq!(shard.runtime, Some(rt.id()));
        assert_eq!(shard.worker, Some(place));
    }
}

#[test]
fn a_runtimes_stats_list_exactly_its_workers_by_place() {
    let (a, b) = (runtime(3), runtime(2));
    assert_its_workers(&a.stats(), &a, 3);
    assert_its_workers(&a.handle().stats(), &a, 3);
    assert_its_workers(&b.stats(), &b, 2);
    assert_its_workers(&b.handle().stats(), &b, 2);
    let (in_a, in_b) = (a.stats(), b.stats());
    assert!(
        in_a.shards.iter().all(|s| s.runtime != Some(b.id())),
        "a's list holds one of b's workers"
    );
    assert!(
        in_b.shards.iter().all(|s| s.runtime != Some(a.id())),
        "b's list holds one of a's workers"
    );
}

#[test]
fn a_runtime_with_no_worker_lists_none() {
    let rt = runtime(0);
    assert!(rt.stats().shards.is_empty());
    assert!(rt.handle().stats().shards.is_empty());
    assert!(
        nuclei::stats()
            .shards
            .iter()
            .all(|s| s.runtime != Some(rt.id()))
    );
}

#[test]
fn the_process_stats_list_every_runtimes_workers_with_their_runtime() {
    let (a, b) = (runtime(2), runtime(1));
    let all = nuclei::stats();
    for (rt, workers) in [(&a, 2), (&b, 1)] {
        let mut places: Vec<_> = all
            .shards
            .iter()
            .filter(|s| s.runtime == Some(rt.id()))
            .map(|s| s.worker)
            .collect();
        places.sort();
        let expected: Vec<_> = (0..workers).map(Some).collect();
        assert_eq!(places, expected);
    }
    for shard in &all.shards {
        assert_eq!(
            shard.runtime.is_some(),
            shard.worker.is_some(),
            "a shard has a runtime exactly when it has a place"
        );
    }
    let first_other = all.shards.iter().position(|s| s.worker.is_none());
    if let Some(at) = first_other {
        assert!(
            all.shards[at..].iter().all(|s| s.worker.is_none()),
            "a worker came after a thread's own shard"
        );
    }
}

#[test]
fn a_runtime_stats_follow_a_worker_that_starts_later() {
    let rt = runtime(1);
    rt.block_on(async {
        assert_eq!(nuclei::spawn_more_threads(1).await.unwrap(), 1);
    });
    wait_for("the second worker's stats", || rt.stats().shards.len() == 2);
    assert_its_workers(&rt.stats(), &rt, 2);
}

#[test]
fn the_blocking_queue_depth_counts_the_jobs_waiting_for_a_thread() {
    let rt = Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .max_blocking_threads(1)
        .build()
        .expect("the runtime starts");
    let gate = Arc::new(Gate::default());
    let (held, started) = (Arc::clone(&gate), Arc::new(AtomicBool::new(false)));
    let up = Arc::clone(&started);
    let first = rt.spawn_blocking(move || {
        up.store(true, Ordering::SeqCst);
        held.hold();
        0
    });
    wait_for("the first job", || started.load(Ordering::SeqCst));
    assert_eq!(rt.stats().blocking_queue_depth, 0);
    let queued: Vec<_> = (1..=3).map(|i| rt.spawn_blocking(move || i)).collect();
    wait_for("three queued jobs", || rt.stats().blocking_queue_depth == 3);
    assert_eq!(rt.handle().stats().blocking_queue_depth, 3);
    assert_eq!(rt.stats().blocking_threads, 1);
    assert!(nuclei::stats().blocking_queue_depth >= 3);

    gate.release_after("the queued jobs");
    assert_eq!(rt.block_on(first), 0);
    let done: Vec<_> = queued.into_iter().map(|job| rt.block_on(job)).collect();
    assert_eq!(done, [1, 2, 3]);
    wait_for("an empty queue", || rt.stats().blocking_queue_depth == 0);
}

#[test]
fn a_task_that_keeps_consuming_budget_counts_budget_yields() {
    let rt = runtime(1);
    assert_eq!(rt.stats().shards[0].budget_yields, 0);
    let rounds = 128 * 40;
    rt.block_on(rt.spawn(async move {
        for _ in 0..rounds {
            nuclei::task::coop::consume_budget().await;
        }
    }));
    wait_for("the budget yields", || {
        rt.stats().shards[0].budget_yields >= 20
    });
    let yields = rt.handle().stats().shards[0].budget_yields;
    assert!(
        yields <= rounds as u64,
        "{yields} yields for {rounds} rounds"
    );
}

#[test]
fn a_task_that_spends_no_budget_yields_for_none() {
    let rt = runtime(1);
    rt.block_on(rt.spawn(async {
        for _ in 0..10 {
            nuclei::task::yield_now().await;
        }
    }));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(rt.stats().shards[0].budget_yields, 0);
}

#[test]
fn poll_times_are_there_only_when_latencies_are_recorded() {
    let rec = recording(2);
    let plain = runtime(2);
    for shard in &rec.stats().shards {
        assert!(shard.poll_times.is_some());
    }
    for rt in [&rec, &plain] {
        let tasks: Vec<_> = (0..30)
            .map(|_| {
                rt.spawn(async {
                    for _ in 0..3 {
                        nuclei::task::yield_now().await;
                    }
                })
            })
            .collect();
        for task in tasks {
            rt.block_on(task);
        }
    }
    // Each task is polled four times: its first poll and one per yield.
    wait_for("the poll times", || {
        let counted: u64 = rec
            .stats()
            .shards
            .iter()
            .filter_map(|s| s.poll_times.as_ref())
            .map(Histogram::count)
            .sum();
        counted >= 30 * 4
    });
    for shard in &plain.stats().shards {
        assert!(shard.poll_times.is_none());
    }
    for shard in &plain.handle().stats().shards {
        assert!(shard.poll_times.is_none());
    }
}

#[test]
fn a_histograms_buckets_are_increasing_powers_of_two_that_add_up_to_its_count() {
    let rt = recording(1);
    rt.block_on(rt.spawn(async {
        for _ in 0..20 {
            nuclei::task::yield_now().await;
        }
        std::thread::sleep(Duration::from_millis(2));
    }));
    let histogram = || rt.stats().shards[0].poll_times.clone().unwrap();
    wait_for("the long poll in a bucket", || {
        histogram()
            .buckets()
            .any(|(low, _, n)| low >= Duration::from_millis(1) && n >= 1)
    });
    let histogram = histogram();
    let buckets: Vec<_> = histogram.buckets().collect();
    assert!(buckets.len() >= 2, "{histogram:?}");
    let mut sum = 0;
    let mut previous_high = Duration::ZERO;
    for (low, high, n) in buckets {
        assert!(n > 0);
        assert!(
            low == Duration::ZERO || low.as_nanos().is_power_of_two(),
            "{low:?}"
        );
        assert!(high.as_nanos().is_power_of_two(), "{high:?}");
        assert!(low < high);
        assert!(low >= previous_high, "{low:?} overlaps {previous_high:?}");
        previous_high = high;
        sum += n;
    }
    assert_eq!(sum, histogram.count());
    assert!(histogram.count() >= 21, "a task polled 21 times at least");
}
