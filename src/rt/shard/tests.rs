use std::sync::Arc;
use std::task::{Wake, Waker};
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

/// A task's timer comes due while the shard is on its way to park: the
/// drive that fires it finds the task it woke, and only looks at the
/// poller instead of waiting there, where nothing would wake the shard.
/// The shard runs on a thread of its own, so a drive that waited fails the
/// test rather than hanging it.
#[test]
fn a_timer_fired_on_the_way_to_park_runs_its_task_rather_than_parking() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let shard = Shard::new(Limits {
            max_nesting: 8,
            buffer_cap: crate::buf::DEFAULT_CAP,
            backend: crate::rt::backend::chosen().unwrap(),
        })
        .unwrap();
        let _current = context::enter(&shard);
        let woke = std::rc::Rc::new(core::cell::Cell::new(false));
        let flag = std::rc::Rc::clone(&woke);
        crate::runtime::spawn_local(async move {
            crate::time::sleep(Duration::from_millis(1)).await;
            flag.set(true);
        })
        .detach();
        // The task arms its timer, which then comes due without firing.
        shard.run_ready();
        let armed_unfired = !woke.get();
        std::thread::sleep(Duration::from_millis(5));
        // Nothing but the timer has work for the shard: a drive that waited
        // on the poller after firing it would never return.
        shard.drive(None).unwrap();
        shard.run_ready();
        let _ = done.send((armed_unfired, woke.get()));
    });
    let (armed_unfired, woke) = finished
        .recv_timeout(Duration::from_secs(10))
        .expect("the drive waited on the poller with the task it woke still to run");
    assert!(armed_unfired, "the timer fired before it was due");
    assert!(woke, "the task the timer woke never ran");
}

/// A waker that holds a shard's `Shared`, as the one of a worker's loop
/// does.
struct Holds(#[allow(dead_code)] Arc<super::Shared>);

#[expect(
    clippy::manual_noop_waker,
    reason = "the waker holds the Shared, which the no-op waker cannot"
)]
impl Wake for Holds {
    fn wake(self: Arc<Self>) {}
}

/// The waker a worker's loop leaves in its shard's `Shared` (when its look
/// after registering finds it may exit) goes with the shard, so the two do
/// not keep each other.
#[test]
fn a_shard_lets_go_of_the_waker_its_loop_left() {
    let shard = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    })
    .unwrap();
    let shared = Arc::downgrade(&shard.shared);
    let waker = Waker::from(Arc::new(Holds(Arc::clone(&shard.shared))));
    shard.shared.stop_waker.register(&waker);
    drop(waker);
    drop(shard);
    assert!(
        shared.upgrade().is_none(),
        "the shard's Shared was kept by its own loop's waker"
    );
}
