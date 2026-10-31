use std::sync::Arc;
use std::time::Duration;

use super::{Limits, Shard};
use crate::rt::tuning::Tuning;
use crate::sync::internal::{AtomicUsize, Ordering};

fn worker_with_pool(jobs: &Arc<AtomicUsize>) -> Shard {
    let tuning = Tuning {
        blocking: Some(Arc::clone(jobs)),
        ..Tuning::default()
    };
    let limits = Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    };
    Shard::worker(limits, tuning, 0, 0, None).unwrap()
}

#[test]
fn a_blocking_job_out_is_not_work_that_comes_soon() {
    let jobs = Arc::new(AtomicUsize::new(0));
    let shard = worker_with_pool(&jobs);
    assert!(!shard.blocking_out());
    assert!(!shard.work_may_come_soon());
    jobs.store(1, Ordering::SeqCst);
    assert!(shard.blocking_out());
    assert!(!shard.work_may_come_soon());
}

#[test]
fn a_short_park_a_blocking_job_ended_leaves_the_spin_cold() {
    let jobs = Arc::new(AtomicUsize::new(0));
    let shard = worker_with_pool(&jobs);
    shard.slept_for(Duration::from_micros(3), false);
    assert!(!shard.spin.is_hot());
    shard.slept_for(Duration::from_micros(3), true);
    assert!(shard.spin.is_hot());
}

#[test]
fn round_trips_to_the_blocking_pool_leave_the_worker_cold() {
    let rt = crate::runtime::Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .unwrap();
    let hot = rt.block_on(rt.spawn(async {
        // A job longer than any spin: a spin that ran out leaves the shard cold.
        crate::runtime::spawn_blocking(|| std::thread::sleep(Duration::from_millis(5))).await;
        for i in 0..64u32 {
            assert_eq!(crate::runtime::spawn_blocking(move || i).await, i);
        }
        crate::rt::context::current(|shard| shard.map(|shard| shard.spin.is_hot()))
    }));
    assert_eq!(
        hot,
        Some(false),
        "a park the pool cut short made the shard spin"
    );
}
