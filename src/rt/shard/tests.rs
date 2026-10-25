use std::time::{Duration, Instant};

use super::{Limits, Shard};
use crate::rt::context;

/// A timer armed in the shard's wheel, done once it fired.
fn sleep_on(shard: &Shard, at: Instant) -> impl core::future::Future<Output = ()> + '_ {
    let mut key = None;
    core::future::poll_fn(move |cx| {
        let mut wheel = shard.wheel().borrow_mut();
        match key {
            None => {
                key = Some(wheel.insert(shard.wheel_time(at), cx.waker()));
            }
            Some(k) if wheel.is_fired(k) => {
                wheel.remove(k);
                return core::task::Poll::Ready(());
            }
            Some(k) => wheel.set_waker(k, cx.waker()),
        }
        let fired = key.is_some_and(|k| wheel.is_fired(k));
        if fired {
            core::task::Poll::Ready(())
        } else {
            core::task::Poll::Pending
        }
    })
}

#[test]
fn a_timer_fires_from_the_run_loop_and_never_early() {
    let shard = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    })
    .unwrap();
    let _current = context::enter(&shard);
    for ms in [1u64, 5, 30] {
        let start = Instant::now();
        let at = start + Duration::from_millis(ms);
        shard.block_on(sleep_on(&shard, at));
        assert!(Instant::now() >= at, "a {ms} ms timer fired early");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}

#[test]
fn the_loop_parks_until_the_next_timer_rather_than_forever() {
    let shard = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    })
    .unwrap();
    let _current = context::enter(&shard);
    let start = Instant::now();
    // Nothing but the timer can wake the loop: if it parked without a
    // deadline, this would never return.
    shard.block_on(sleep_on(&shard, start + Duration::from_millis(20)));
    assert!(start.elapsed() >= Duration::from_millis(20));
}
