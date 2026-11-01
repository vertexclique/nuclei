//! A write the blocking pool makes reaches its task only once the job's task
//! is gone: by then the job's block is back with the thread that spawned it,
//! so that thread's next spawn finds it (proofs/tla/jobend).

use core::future::poll_fn;
use core::task::Poll;

use crate::fs::call::{FileOutcome, SlotJob, write_on_pool};

/// Writes made one after another, each awaited before the next.
const WRITES: usize = 8;

#[test]
fn a_pool_write_ends_after_its_jobs_block_went_back() {
    crate::block_on(async {
        for _ in 0..WRITES {
            // The job takes a block of this thread's (from its cache, or new
            // when the cache has none of its class).
            let before = crate::mem::held();
            let mut job: SlotJob<std::io::Result<()>> =
                poll_fn(|cx| Poll::Ready(write_on_pool(|| FileOutcome::Write(Ok(())), cx.waker())))
                    .await;
            poll_fn(|cx| job.poll(cx.waker())).await.unwrap();
            assert!(
                crate::mem::held() >= before,
                "the write's end came before its job's block went back"
            );
        }
    });
}
