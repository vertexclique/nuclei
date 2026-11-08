//! The cost of a park that leaves kovan's reservation first: a task and a blocking thread passing
//! one value back and forth (every turn parks both sides), and tasks on different shards waking
//! each other in a ring (every turn parks the shard that waits).
//!
//! Run with `cargo bench --bench park`.
//!
//! - **pingpong**: one element is one round trip between a task and a thread blocked in
//!   `blocking_recv`.
//! - **pingpong_deadline**: the same with the thread in `blocking_recv_deadline`, a far deadline.
//! - **wake_storm**: one element is one hop of a value around a ring of tasks, one per shard.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use nuclei::runtime::{Builder, Runtime};
use nuclei::sync::mpsc;

const TRIPS: u64 = 2_000;
const SHARDS: usize = 4;
const LAPS: u64 = 500;

fn runtime(workers: usize) -> Runtime {
    Builder::new()
        .worker_threads(workers)
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .expect("the runtime starts")
}

fn pingpong(c: &mut Criterion) {
    let rt = runtime(1);
    let mut group = c.benchmark_group("park");
    group.throughput(Throughput::Elements(TRIPS));
    group.bench_function("pingpong", |b| {
        b.iter(|| {
            let (to_thread, mut thread_rx) = mpsc::unbounded_channel::<u64>();
            let (to_task, mut task_rx) = mpsc::unbounded_channel::<u64>();
            let thread = std::thread::spawn(move || {
                while let Some(n) = thread_rx.blocking_recv() {
                    if to_task.send(n).is_err() {
                        break;
                    }
                }
            });
            rt.block_on(async {
                for n in 0..TRIPS {
                    to_thread.send(n).unwrap();
                    assert_eq!(task_rx.recv().await, Some(n));
                }
            });
            drop(to_thread);
            thread.join().unwrap();
        });
    });
    group.finish();
}

fn pingpong_deadline(c: &mut Criterion) {
    let rt = runtime(1);
    let mut group = c.benchmark_group("park");
    group.throughput(Throughput::Elements(TRIPS));
    group.bench_function("pingpong_deadline", |b| {
        b.iter(|| {
            let (to_thread, mut thread_rx) = mpsc::unbounded_channel::<u64>();
            let (to_task, mut task_rx) = mpsc::unbounded_channel::<u64>();
            let thread = std::thread::spawn(move || {
                let far = std::time::Duration::from_secs(60);
                while let Ok(n) = thread_rx.blocking_recv_deadline(std::time::Instant::now() + far)
                {
                    if to_task.send(n).is_err() {
                        break;
                    }
                }
            });
            rt.block_on(async {
                for n in 0..TRIPS {
                    to_thread.send(n).unwrap();
                    assert_eq!(task_rx.recv().await, Some(n));
                }
            });
            drop(to_thread);
            thread.join().unwrap();
        });
    });
    group.finish();
}

fn wake_storm(c: &mut Criterion) {
    let rt = runtime(SHARDS);
    let mut group = c.benchmark_group("park");
    group.throughput(Throughput::Elements(LAPS * SHARDS as u64));
    group.bench_function("wake_storm", |b| {
        b.iter(|| {
            let (txs, rxs): (Vec<_>, Vec<_>) = (0..SHARDS)
                .map(|_| mpsc::unbounded_channel::<u64>())
                .unzip();
            let first = txs[0].clone();
            let tasks: Vec<_> = rxs
                .into_iter()
                .enumerate()
                .map(|(i, mut rx)| {
                    let next = txs[(i + 1) % SHARDS].clone();
                    rt.spawn(async move {
                        while let Some(lap) = rx.recv().await {
                            if lap == LAPS && i == SHARDS - 1 {
                                return;
                            }
                            let lap = if i == SHARDS - 1 { lap + 1 } else { lap };
                            if next.send(lap).is_err() {
                                return;
                            }
                        }
                    })
                })
                .collect();
            drop(txs);
            first.send(1).unwrap();
            drop(first);
            for task in tasks {
                rt.block_on(task);
            }
        });
    });
    group.finish();
}

criterion_group!(benches, pingpong, pingpong_deadline, wake_storm);
criterion_main!(benches);
