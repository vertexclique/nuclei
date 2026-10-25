//! The bench macro, compiled against the runtime and run: benchmarks need
//! the nightly test harness, so this builds only with `--cfg nuclei_nightly`
//! on a nightly toolchain (the CI's nightly job). Run as tests, each
//! benchmark's body runs once; `cargo bench` times many runs.

#![cfg(nuclei_nightly)]
#![feature(test)]

extern crate test;

use std::sync::atomic::{AtomicUsize, Ordering};

static DEFAULT_RUNS: AtomicUsize = AtomicUsize::new(0);
static OWN_RUNS: AtomicUsize = AtomicUsize::new(0);

#[nuclei::bench]
async fn a_task_spawned_and_awaited_on_the_default_runtime() {
    DEFAULT_RUNS.fetch_add(1, Ordering::Relaxed);
    assert_eq!(nuclei::spawn(async { 1 }).await, 1);
}

#[nuclei::bench(threads = 2)]
async fn a_task_spawned_and_awaited_on_a_runtime_of_its_own() {
    OWN_RUNS.fetch_add(1, Ordering::Relaxed);
    assert_eq!(nuclei::spawn(async { 2 }).await, 2);
}
