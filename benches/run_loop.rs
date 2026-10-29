//! The cost of a task run through a worker shard's run loop: many tasks that
//! only yield, on a runtime of one worker, so the time per element is the
//! loop's own work between two polls (taking the next task, counting the run,
//! the checks for a look at the driver) and the poll of a task that does
//! nothing.
//!
//! Run with `cargo bench --bench run_loop`; pin it to one core for steady
//! numbers (`taskset -c 2 cargo bench --bench run_loop`).
//!
//! - **yield**: 64 tasks each yield 4,000 times; one element is one task run.
//! - **spawn_join**: 1,000 tasks spawned and awaited, each running once; one
//!   element is one task.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use nuclei::runtime::{Builder, Runtime};

const TASKS: u64 = 64;
const YIELDS: u64 = 4_000;
const SPAWNED: u64 = 1_000;

fn runtime() -> Runtime {
    Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .expect("the runtime starts")
}

fn rows(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("run_loop");
    group.throughput(Throughput::Elements(TASKS * YIELDS));
    group.bench_function("yield", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..TASKS)
                .map(|_| {
                    rt.spawn(async {
                        for _ in 0..YIELDS {
                            nuclei::task::yield_now().await;
                        }
                    })
                })
                .collect();
            for task in tasks {
                rt.block_on(task);
            }
        });
    });
    group.throughput(Throughput::Elements(SPAWNED));
    group.bench_function("spawn_join", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..SPAWNED).map(|i| rt.spawn(async move { i })).collect();
            for task in tasks {
                rt.block_on(task);
            }
        });
    });
    group.finish();
}

criterion_group!(benches, rows);
criterion_main!(benches);
