//! `#[nuclei::test(builder = "path")]` builds the runtime from the named
//! function, with `threads` and `start_paused` applied on top when given.

use std::thread;
use std::time::Duration;

use nuclei::runtime::RuntimeHandle;

fn three() -> nuclei::Builder {
    nuclei::Builder::new().worker_threads(3).pin_workers(false)
}

mod setup {
    pub fn named() -> nuclei::Builder {
        nuclei::Builder::new()
            .worker_threads(2)
            .pin_workers(false)
            .thread_name("macro-builder")
    }
}

mod reexport {
    pub use nuclei;
}

async fn worker_thread_name() -> Option<String> {
    nuclei::spawn(async { thread::current().name().map(str::to_owned) }).await
}

#[nuclei::test(builder = "three")]
async fn the_runtime_comes_from_the_function() {
    assert_eq!(RuntimeHandle::current().workers(), 3);
    assert_eq!(nuclei::spawn(async { 6 * 7 }).await, 42);
}

#[nuclei::test(builder = "three", threads = 1)]
async fn threads_replaces_the_workers_the_function_chose() {
    assert_eq!(RuntimeHandle::current().workers(), 1);
}

#[nuclei::test(builder = "three", threads = 0)]
async fn threads_zero_runs_everything_on_the_tests_own_thread() {
    assert_eq!(RuntimeHandle::current().workers(), 0);
    let me = thread::current().id();
    assert_eq!(nuclei::spawn(async { thread::current().id() }).await, me);
}

#[nuclei::test(builder = "setup::named")]
async fn the_function_may_live_in_a_module_and_configure_more_than_the_workers() {
    assert_eq!(RuntimeHandle::current().workers(), 2);
    assert_eq!(worker_thread_name().await.as_deref(), Some("macro-builder"));
}

#[nuclei::test(builder = "crate::setup::named")]
async fn a_path_may_start_at_crate() {
    assert_eq!(RuntimeHandle::current().workers(), 2);
    assert_eq!(worker_thread_name().await.as_deref(), Some("macro-builder"));
}

#[nuclei::test(builder = "self::setup::named")]
async fn a_path_may_start_at_self() {
    assert_eq!(RuntimeHandle::current().workers(), 2);
}

#[nuclei::test(builder = "::setup::named")]
async fn a_leading_colon_pair_is_dropped() {
    assert_eq!(RuntimeHandle::current().workers(), 2);
}

#[nuclei::test(builder = "three", start_paused = true)]
async fn start_paused_keeps_the_functions_workers_and_pauses_the_clock() {
    assert_eq!(RuntimeHandle::current().workers(), 3);
    let began = std::time::Instant::now();
    let start = nuclei::time::Instant::now();
    nuclei::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(start.elapsed(), Duration::from_secs(3600));
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "the hour was not skipped"
    );
}

#[nuclei::test(builder = "setup::named", threads = 3, start_paused = true)]
async fn the_options_apply_together() {
    assert_eq!(RuntimeHandle::current().workers(), 3);
    let start = nuclei::time::Instant::now();
    nuclei::time::sleep(Duration::from_secs(60)).await;
    assert!(start.elapsed() >= Duration::from_secs(60));
}

#[nuclei::test(builder = "setup::named", crate = "crate::reexport::nuclei")]
async fn it_goes_with_a_crate_path_and_a_result() -> std::io::Result<()> {
    assert_eq!(RuntimeHandle::current().workers(), 2);
    assert_eq!(crate::reexport::nuclei::spawn(async { 1 }).await, 1);
    Ok(())
}
