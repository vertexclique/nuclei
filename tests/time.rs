//! Timers through the public surface: `sleep`, `sleep_until`, `timeout`,
//! `timeout_at` and `interval`, on a shard and from threads that run none.

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use nuclei::time::Instant;

use nuclei::runtime::{Runtime, spawn};
use nuclei::time::{self, MissedTickBehavior};

fn runtime(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

/// A minimal executor of a thread that is not a shard: park until woken.
fn foreign_block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread, AtomicBool);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.1.store(true, Ordering::SeqCst);
            self.0.unpark();
        }
    }
    let unpark = Arc::new(Unpark(std::thread::current(), AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&unpark));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        while !unpark.1.swap(false, Ordering::SeqCst) {
            std::thread::park();
        }
    }
}

#[test]
fn a_sleep_waits_at_least_its_duration() {
    let rt = runtime(1);
    rt.block_on(async {
        for ms in [0u64, 1, 3, 10, 25] {
            let start = Instant::now();
            time::sleep(Duration::from_millis(ms)).await;
            let took = start.elapsed();
            assert!(took >= Duration::from_millis(ms), "{ms} ms took {took:?}");
            assert!(took < Duration::from_secs(5));
        }
    });
}

#[test]
fn a_deadline_in_the_past_is_ready_at_once() {
    let rt = runtime(1);
    rt.block_on(async {
        let past = Instant::now() - Duration::from_millis(5);
        let start = Instant::now();
        time::sleep_until(past).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    });
}

#[test]
fn sleeps_on_the_workers_wake_their_tasks() {
    let rt = runtime(4);
    let starts: Vec<_> = (0..64u64)
        .map(|i| {
            rt.spawn(async move {
                let start = Instant::now();
                let d = Duration::from_millis(1 + i % 7);
                time::sleep(d).await;
                start.elapsed() >= d
            })
        })
        .collect();
    let all = rt.block_on(async {
        let mut all = true;
        for t in starts {
            all &= t.await;
        }
        all
    });
    assert!(all, "some sleep ended early");
}

#[test]
fn a_timeout_returns_the_output_of_a_future_that_finishes_first() {
    let rt = runtime(1);
    let out = rt.block_on(time::timeout(Duration::from_secs(10), async { 7 }));
    assert_eq!(out, Ok(7));
}

#[test]
fn a_ready_future_wins_even_a_timeout_of_zero() {
    let rt = runtime(1);
    assert_eq!(
        rt.block_on(time::timeout(Duration::ZERO, async { 1 })),
        Ok(1)
    );
}

#[test]
fn a_timeout_elapses_on_a_future_that_never_finishes() {
    let rt = runtime(1);
    let start = Instant::now();
    let out = rt.block_on(time::timeout(
        Duration::from_millis(20),
        std::future::pending::<()>(),
    ));
    // `Elapsed` is `Copy` and compares equal to itself.
    assert_eq!(out, Err(out.unwrap_err()));
    assert!(start.elapsed() >= Duration::from_millis(20));
    let err: std::io::Error = out.unwrap_err().into();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(out.unwrap_err().to_string(), "deadline has elapsed");
}

#[test]
fn an_interval_ticks_at_its_period_from_an_immediate_first_tick() {
    let rt = runtime(1);
    rt.block_on(async {
        let period = Duration::from_millis(5);
        let mut iv = time::interval(period);
        let start = Instant::now();
        let first = iv.tick().await;
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "the first tick is at once"
        );
        let mut last = first;
        for _ in 0..5 {
            let due = iv.tick().await;
            assert_eq!(due - last, period, "ticks are due one period apart");
            assert!(Instant::now() >= due);
            last = due;
        }
    });
}

#[test]
fn missed_ticks_burst_delay_or_skip() {
    let rt = runtime(1);
    rt.block_on(async {
        let period = Duration::from_millis(10);
        for behavior in [
            MissedTickBehavior::Burst,
            MissedTickBehavior::Delay,
            MissedTickBehavior::Skip,
        ] {
            let mut iv = time::interval(period);
            iv.set_missed_tick_behavior(behavior);
            assert_eq!(iv.missed_tick_behavior(), behavior);
            let start = iv.tick().await;
            // Miss about three ticks.
            std::thread::sleep(Duration::from_millis(35));
            let late = iv.tick().await;
            // Delay moves the schedule from when the first tick ran.
            if behavior == MissedTickBehavior::Delay {
                assert!(late >= start + period);
            } else {
                assert_eq!(late, start + period, "the missed tick is reported as due");
            }
            let polled = Instant::now();
            let next = iv.tick().await;
            match behavior {
                MissedTickBehavior::Burst => {
                    assert_eq!(next, start + 2 * period);
                    assert!(
                        polled.elapsed() < Duration::from_millis(9),
                        "a burst catches up"
                    );
                }
                MissedTickBehavior::Delay => {
                    assert!(next >= polled + period - Duration::from_millis(1));
                }
                MissedTickBehavior::Skip => {
                    assert!(
                        next > start + 3 * period,
                        "skipped to the schedule after now"
                    );
                    assert_eq!((next - start).as_millis() % period.as_millis(), 0);
                }
            }
        }
    });
}

#[test]
fn a_reset_sleep_waits_for_its_new_deadline() {
    let rt = runtime(1);
    rt.block_on(async {
        let start = Instant::now();
        let mut s = Box::pin(time::sleep(Duration::from_secs(3600)));
        s.as_mut().reset(start + Duration::from_millis(10));
        s.as_mut().await;
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(10) && took < Duration::from_secs(5));
    });
}

#[test]
fn a_sleep_works_on_a_thread_that_runs_no_shard() {
    let start = Instant::now();
    foreign_block_on(time::sleep(Duration::from_millis(15)));
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(15), "{took:?}");
    assert!(took < Duration::from_secs(5));
}

#[test]
fn a_timeout_works_on_a_thread_that_runs_no_shard() {
    let out = foreign_block_on(time::timeout(
        Duration::from_millis(10),
        std::future::pending::<()>(),
    ));
    assert!(out.is_err());
}

#[test]
fn a_dropped_foreign_sleep_never_wakes_its_task() {
    // Armed for this thread, then dropped: a cancelled timer must not reach
    // a waker that is gone.
    let woke = Arc::new(AtomicBool::new(false));
    struct Flag(Arc<AtomicBool>);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let waker = Waker::from(Arc::new(Flag(Arc::clone(&woke))));
    let mut cx = Context::from_waker(&waker);
    // The deadline leaves the drop far ahead of it even on a loaded
    // machine: a timer that fires before its sleep is dropped wakes the task
    // rightly, and is not what this checks.
    let deadline = Duration::from_millis(50);
    let armed = std::time::Instant::now();
    let mut s = Box::pin(time::sleep(deadline));
    assert!(s.as_mut().poll(&mut cx).is_pending());
    drop(s);
    let dropped_after = armed.elapsed();
    // Past the deadline: a timer left armed would have fired by now.
    std::thread::sleep(Duration::from_millis(120));
    // A drop that came only after the deadline (a stalled machine) may have
    // been beaten by the fire, which wakes the task rightly: only a drop
    // before it says anything.
    if dropped_after < deadline {
        assert!(
            !woke.load(Ordering::SeqCst),
            "a dropped sleep woke its task"
        );
    }
}

#[test]
// The task hands the armed sleep out on purpose, to finish it elsewhere.
#[allow(clippy::async_yields_async)]
fn a_sleep_armed_on_one_runtime_finishes_on_another() {
    let a = runtime(1);
    let b = runtime(1);
    let sleep = a.block_on(a.spawn(async {
        let mut s = Box::pin(time::sleep(Duration::from_millis(20)));
        // Armed on this worker's wheel, then handed away still pending.
        let poll = std::future::poll_fn(|cx| Poll::Ready(s.as_mut().poll(cx))).await;
        assert!(poll.is_pending());
        s
    }));
    let start = Instant::now();
    b.block_on(b.spawn(sleep));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn thousands_of_timers_all_fire_and_none_early() {
    let rt = runtime(4);
    let n = 2000u64;
    let tasks: Vec<_> = (0..n)
        .map(|i| {
            rt.spawn(async move {
                let d = Duration::from_micros(500 + (i * 7919) % 30_000);
                let start = Instant::now();
                time::sleep(d).await;
                start.elapsed() >= d
            })
        })
        .collect();
    let ok = rt.block_on(async {
        let mut ok = 0u64;
        for t in tasks {
            ok += u64::from(t.await);
        }
        ok
    });
    assert_eq!(ok, n);
}

#[test]
fn sleep_and_timeout_are_at_the_crate_root() {
    let rt = runtime(1);
    rt.block_on(async {
        nuclei::sleep(Duration::from_millis(1)).await;
        assert!(
            nuclei::timeout(Duration::from_secs(1), async {})
                .await
                .is_ok()
        );
        spawn(async {}).await;
    });
}

/// A sleep is not tied to its first poll's task: polled from another task
/// on the same shard, it wakes that one.
#[test]
fn a_sleep_wakes_whichever_task_polled_it_last() {
    let rt = runtime(1);
    rt.block_on(async {
        let mut s = Box::pin(time::sleep(Duration::from_millis(10)));
        let first = std::future::poll_fn(|cx| Poll::Ready(s.as_mut().poll(cx))).await;
        assert!(first.is_pending());
        spawn(s).await;
    });
}

#[allow(dead_code)]
fn sleep_is_send_sync_unpin(s: time::Sleep) -> impl Send + Sync + Unpin {
    s
}

#[allow(dead_code)]
fn pin_projection_compiles(_t: Pin<&mut time::Timeout<std::future::Pending<()>>>) {}
