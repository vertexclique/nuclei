//! Tests of `nuclei::sync::local`: the single-shard semaphore, mutex,
//! channels and one-shot channel. The futures are driven by hand under a
//! `Probe` to pin wakeups and cancellation exactly; the budget and the
//! many-task cases run on a one-worker runtime.
#![cfg(not(target_arch = "wasm32"))]

mod sync_support;

#[path = "sync_local/budget.rs"]
mod budget;
#[path = "sync_local/cancellation.rs"]
mod cancellation;
#[path = "sync_local/mpsc_bounded.rs"]
mod mpsc_bounded;
#[path = "sync_local/mpsc_unbounded.rs"]
mod mpsc_unbounded;
#[path = "sync_local/mutex.rs"]
mod mutex;
#[path = "sync_local/oneshot.rs"]
mod oneshot;
#[path = "sync_local/semaphore.rs"]
mod semaphore;
#[path = "sync_local/util.rs"]
mod util;
