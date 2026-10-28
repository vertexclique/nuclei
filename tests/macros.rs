//! The test macro's expansions, compiled against the runtime and run: on the
//! process default runtime, on a runtime with no workers, on one with
//! workers, on a paused clock, and through a re-exported path. `main` is in `macros_main.rs`, the bench macro in
//! `macros_bench.rs`.

use std::thread;

#[nuclei::test]
async fn a_test_runs_on_the_default_runtime() {
    assert_eq!(nuclei::spawn(async { 6 * 7 }).await, 42);
}

#[nuclei::test(threads = 0)]
async fn a_test_with_no_workers_runs_its_tasks_on_its_own_thread() {
    let me = thread::current().id();
    assert_eq!(nuclei::spawn(async { thread::current().id() }).await, me);
}

#[nuclei::test(threads = 2)]
async fn a_test_with_workers_runs_its_tasks_on_them() -> std::io::Result<()> {
    let tasks: Vec<_> = (0..8)
        .map(|_| nuclei::spawn(async { thread::current().name().map(str::to_owned) }))
        .collect();
    for task in tasks {
        assert_eq!(task.await.as_deref(), Some("nuclei-worker"));
    }
    Ok(())
}

#[nuclei::test(start_paused = true)]
async fn a_paused_test_skips_its_sleeps_and_runs_on_its_own_thread() {
    use std::time::Duration;
    let began = std::time::Instant::now();
    let start = nuclei::time::Instant::now();
    nuclei::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(start.elapsed(), Duration::from_secs(3600));
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "the hour was not skipped"
    );
    let me = thread::current().id();
    assert_eq!(nuclei::spawn(async { thread::current().id() }).await, me);
}

#[nuclei::test(start_paused = true, threads = 2)]
async fn a_paused_test_with_workers_jumps_over_every_shard() {
    use std::time::Duration;
    let start = nuclei::time::Instant::now();
    let tasks: Vec<_> = (1..=4u64)
        .map(|i| {
            nuclei::spawn(async move {
                nuclei::time::sleep(Duration::from_secs(60 * i)).await;
            })
        })
        .collect();
    for task in tasks {
        task.await;
    }
    assert!(start.elapsed() >= Duration::from_secs(240));
}

/// nuclei under another path, as a crate that re-exports it would have it.
mod reexport {
    pub use nuclei;
}

#[nuclei::test(crate = "crate::reexport::nuclei")]
async fn a_test_reaches_nuclei_through_the_path_crate_names() {
    assert_eq!(nuclei::spawn(async { 1 + 1 }).await, 2);
}
