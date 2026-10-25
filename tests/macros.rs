//! The test macro's expansions, compiled against the runtime and run: on the
//! process default runtime, on a runtime with no workers, and on one with
//! workers. `main` is in `macros_main.rs`, the bench macro in
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
