//! The IO knobs reach what reads them: the handles of a runtime's tasks and of a thread inside
//! its `block_on` read its values, a thread outside every runtime the defaults, and each
//! worker's ring takes the zero-copy threshold.

use std::time::Duration;

use super::{IoKnobs, current};
use crate::runtime::Builder;

#[test]
fn a_runtimes_threads_read_its_knobs_and_others_the_defaults() {
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .write_behind_linger(Duration::from_secs(3))
        .send_file_chunk(4096)
        .build()
        .unwrap();
    let inside = rt.block_on(async { current() });
    assert_eq!(inside.linger, Duration::from_secs(3));
    assert_eq!(inside.send_file_chunk, 4096);
    let on_worker = rt.block_on(rt.spawn(async { (current(), crate::rt::linger::time()) }));
    assert_eq!(on_worker.0.send_file_chunk, 4096);
    assert_eq!(on_worker.1, Duration::from_secs(3));
    let default = IoKnobs::default();
    let outside = std::thread::spawn(current).join().unwrap();
    assert_eq!(
        (outside.linger, outside.send_file_chunk),
        (default.linger, default.send_file_chunk)
    );
    assert_eq!(default.linger, crate::io::handle::LINGER);
    assert_eq!(
        default.send_file_chunk,
        crate::net::send_file::SEND_FILE_CHUNK
    );
}

#[cfg(ring)]
#[test]
fn each_workers_ring_takes_the_zero_copy_threshold() {
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .zero_copy_threshold(4096)
        .build()
        .unwrap();
    /// The threshold of this thread's ring, if its shard runs one.
    fn threshold() -> Option<usize> {
        crate::rt::context::current(|s| s.and_then(|s| s.ring()).map(|r| r.zero_copy_min()))
    }
    let on_worker = rt.block_on(rt.spawn(async { threshold() }));
    // A worker that runs the poller (the environment may name it) has no ring to set.
    assert!(matches!(on_worker, None | Some(4096)), "{on_worker:?}");
    let own = rt.block_on(async { threshold() });
    assert!(
        matches!(own, None | Some(crate::io::source::ring::SEND_ZC_MIN)),
        "a thread's own shard took a runtime's threshold: {own:?}"
    );
}
