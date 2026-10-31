use core::future::Future;
use core::pin::Pin;
use core::task::Poll;
use std::time::Duration;

use super::sleep;
use crate::rt::context;
use crate::rt::shard::{Limits, Shard};

fn shard() -> Shard {
    Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    })
    .unwrap()
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
// The block hands the armed sleep out on purpose, to drop it elsewhere.
#[allow(clippy::async_yields_async)]
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

#[test]
fn a_dropped_precise_sleep_leaves_nothing_armed() {
    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        let mut s = super::sleep_precise(Duration::from_secs(3600));
        assert!(poll_once(&mut s).await.is_pending());
        assert_eq!(shard.wheel_of(true).borrow().armed(), 1);
        assert_eq!(shard.wheel().borrow().armed(), 0);
        drop(s);
        assert_eq!(shard.wheel_of(true).borrow().armed(), 0);
    });
}

#[test]
fn a_finished_precise_sleep_leaves_nothing_in_its_wheel() {
    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        super::sleep_precise(Duration::from_micros(300)).await;
        assert_eq!(shard.wheel_of(true).borrow().armed(), 0);
    });
}

#[test]
// The block hands the armed sleep out on purpose, to drop it elsewhere.
#[allow(clippy::async_yields_async)]
fn a_precise_sleep_dropped_on_another_thread_is_removed_by_its_shard() {
    let shard = shard();
    let _current = context::enter(&shard);
    let s = shard.block_on(async {
        let mut s = super::sleep_precise(Duration::from_secs(3600));
        assert!(poll_once(&mut s).await.is_pending());
        s
    });
    assert_eq!(shard.wheel_of(true).borrow().armed(), 1);
    std::thread::spawn(move || drop(s)).join().unwrap();
    shard.block_on(async {
        super::sleep_precise(Duration::from_micros(100)).await;
    });
    assert_eq!(shard.wheel_of(true).borrow().armed(), 0);
}

#[test]
fn a_reset_precise_sleep_is_armed_once_at_its_new_deadline() {
    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        let mut s = super::sleep_precise(Duration::from_secs(3600));
        assert!(poll_once(&mut s).await.is_pending());
        let soon = super::Instant::now() + Duration::from_micros(200);
        Pin::new(&mut s).reset(soon);
        assert_eq!(shard.wheel_of(true).borrow().armed(), 0);
        assert_eq!(s.deadline(), soon);
        (&mut s).await;
        assert!(s.is_elapsed());
        assert!(super::Instant::now() >= soon);
        assert_eq!(shard.wheel_of(true).borrow().armed(), 0);
    });
}

#[test]
fn the_microsecond_wheel_rounds_a_deadline_up_and_never_early() {
    let shard = shard();
    let at = crate::time::clock::origin() + Duration::from_nanos(2_000_001);
    assert_eq!(shard.time_of(true, at), 2_001);
    assert_eq!(shard.time_of(false, at), 3);
    let exact = crate::time::clock::origin() + Duration::from_micros(7);
    assert_eq!(shard.time_of(true, exact), 7);
}
