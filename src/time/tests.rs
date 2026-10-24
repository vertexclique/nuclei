use core::future::Future;
use core::pin::Pin;
use core::task::Poll;
use std::time::Duration;

use super::sleep;
use crate::rt::context;
use crate::rt::shard::{Limits, Shard};

fn shard() -> Shard {
    Shard::new(Limits { max_nesting: 8 }).unwrap()
}

/// Polls `f` once, from inside the shard's loop.
async fn poll_once<F: Future + Unpin>(f: &mut F) -> Poll<F::Output> {
    core::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut *f).poll(cx))).await
}

#[test]
fn a_dropped_sleep_leaves_nothing_armed() {
    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        let mut s = sleep(Duration::from_secs(3600));
        assert!(poll_once(&mut s).await.is_pending());
        assert_eq!(shard.wheel().borrow().armed(), 1);
        drop(s);
        assert_eq!(shard.wheel().borrow().armed(), 0);
    });
}

#[test]
fn a_finished_sleep_leaves_nothing_in_the_wheel() {
    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        sleep(Duration::from_millis(2)).await;
        assert_eq!(shard.wheel().borrow().armed(), 0);
    });
}

#[test]
fn a_sleep_dropped_on_another_thread_is_removed_by_its_shard() {
    let shard = shard();
    let _current = context::enter(&shard);
    let s = shard.block_on(async {
        let mut s = sleep(Duration::from_secs(3600));
        assert!(poll_once(&mut s).await.is_pending());
        s
    });
    assert_eq!(shard.wheel().borrow().armed(), 1);
    std::thread::spawn(move || drop(s)).join().unwrap();
    // The removal waits in the shard's mailbox until its next look.
    shard.block_on(async {
        sleep(Duration::from_millis(1)).await;
    });
    assert_eq!(shard.wheel().borrow().armed(), 0);
}
