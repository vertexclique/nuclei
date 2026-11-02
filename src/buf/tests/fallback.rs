//! The pool of a thread that runs no shard holds no more than the `io_buffer_cap` of the runtime
//! the thread serves, as a worker's does (proofs/tla/bufcap/BufCap.tla).

use crate::runtime::{Builder, Runtime};

/// What this thread's pool may hold, asked from a thread that runs no shard.
fn fallback_cap() -> usize {
    super::super::with_pool(|pool| pool.expect("the thread's pool").cap())
}

/// A cap below the default that a worker of this process's backend starts with, and a runtime
/// built with it.
fn small() -> (usize, Runtime) {
    let cap = crate::rt::shard::least_buffer_cap() + (1 << 20);
    assert!(cap < super::super::DEFAULT_CAP, "the cap is the default");
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .io_buffer_cap(cap)
        .build()
        .unwrap();
    (cap, rt)
}

#[test]
fn a_blocking_pool_thread_holds_its_runtime_cap() {
    let (cap, rt) = small();
    let held = rt.block_on(rt.spawn_blocking(fallback_cap));
    assert_eq!(
        held, cap,
        "a pool thread's buffers are not held to the runtime's cap"
    );
}

#[test]
fn a_thread_inside_a_runtime_holds_its_cap() {
    let (cap, rt) = small();
    let held = std::thread::scope(|s| {
        s.spawn(|| {
            let _inside = rt.enter();
            fallback_cap()
        })
        .join()
        .unwrap()
    });
    assert_eq!(
        held, cap,
        "a thread inside the runtime is not held to its cap"
    );
}
