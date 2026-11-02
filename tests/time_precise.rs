//! `time::sleep_precise`: never early, closer to its deadline than a sleep on
//! the millisecond wheel, exact on a paused clock, and working off a shard.

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use nuclei::runtime::Runtime;
use nuclei::time::{self, Instant};

fn runtime(workers: usize, paused: bool) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .start_paused(paused)
        .build()
        .expect("the runtime starts")
}

#[test]
fn a_precise_sleep_never_ends_early() {
    let rt = runtime(1, false);
    rt.block_on(rt.spawn(async {
        for micros in [1, 50, 200, 999, 1_000, 1_300, 4_700] {
            let d = Duration::from_micros(micros);
            let start = std::time::Instant::now();
            time::sleep_precise(d).await;
            assert!(start.elapsed() >= d, "{micros} us ended early");
        }
    }));
}

#[test]
fn a_precise_sleep_in_block_on_never_ends_early() {
    let rt = runtime(0, false);
    rt.block_on(async {
        for micros in [30, 700, 2_100] {
            let d = Duration::from_micros(micros);
            let start = std::time::Instant::now();
            time::sleep_precise(d).await;
            assert!(start.elapsed() >= d, "{micros} us ended early");
        }
    });
}

/// Runs of precise sleeps and of sleeps on the wheel take turns, so the other
/// tests of this binary, running at once on a host of few cores, slow both
/// alike: timed one after the other, the run that met them lost to the run
/// that came after them (an Intel macOS runner, whose precise sleep of 300 us
/// takes 370 us at the median when it runs alone).
#[test]
fn back_to_back_precise_sleeps_do_not_wait_for_the_wheel_millisecond() {
    let rt = runtime(1, false);
    let (precise, coarse) = rt.block_on(rt.spawn(async {
        let d = Duration::from_micros(300);
        let (mut precise, mut coarse) = (Duration::ZERO, Duration::ZERO);
        for _ in 0..5 {
            let start = std::time::Instant::now();
            for _ in 0..6 {
                time::sleep_precise(d).await;
            }
            precise += start.elapsed();
            let start = std::time::Instant::now();
            for _ in 0..6 {
                time::sleep(d).await;
            }
            coarse += start.elapsed();
        }
        (precise, coarse)
    }));
    assert!(
        precise < coarse,
        "30 precise sleeps of 300 us took {precise:?}, 30 on the wheel {coarse:?}"
    );
}

#[test]
fn a_paused_clock_moves_to_a_precise_deadline_exactly() {
    let rt = runtime(0, true);
    rt.block_on(async {
        let start = Instant::now();
        time::sleep_precise(Duration::from_micros(250)).await;
        assert_eq!(start.elapsed(), Duration::from_micros(250));
        time::sleep_until_precise(start + Duration::from_micros(1_337)).await;
        assert_eq!(start.elapsed(), Duration::from_micros(1_337));
    });
}

#[test]
fn a_paused_clock_moves_through_precise_and_wheel_deadlines_in_order() {
    let rt = runtime(1, true);
    let ends = rt.block_on(rt.spawn(async {
        let start = Instant::now();
        let a = nuclei::spawn(async move {
            time::sleep_precise(Duration::from_micros(1_500)).await;
            start.elapsed()
        });
        let b = nuclei::spawn(async move {
            time::sleep(Duration::from_millis(1)).await;
            start.elapsed()
        });
        let c = nuclei::spawn(async move {
            time::sleep_precise(Duration::from_micros(700)).await;
            start.elapsed()
        });
        (a.await, b.await, c.await)
    }));
    assert_eq!(
        ends,
        (
            Duration::from_micros(1_500),
            Duration::from_millis(1),
            Duration::from_micros(700)
        )
    );
}

#[test]
fn advance_fires_a_precise_sleep_at_its_deadline() {
    let rt = runtime(0, false);
    rt.block_on(async {
        time::pause();
        let start = Instant::now();
        let mut sleep = pin!(time::sleep_precise(Duration::from_micros(300)));
        assert!(poll_now(sleep.as_mut()).is_pending());
        time::advance(Duration::from_micros(200)).await;
        assert!(!sleep.is_elapsed());
        assert!(poll_now(sleep.as_mut()).is_pending());
        time::advance(Duration::from_micros(100)).await;
        assert!(sleep.is_elapsed());
        sleep.as_mut().await;
        assert_eq!(start.elapsed(), Duration::from_micros(300));
    });
}

#[test]
fn a_reset_precise_sleep_ends_at_its_new_deadline() {
    let rt = runtime(1, false);
    rt.block_on(rt.spawn(async {
        let mut sleep = pin!(time::sleep_precise(Duration::from_secs(3600)));
        assert!(poll_now(sleep.as_mut()).is_pending());
        let soon = Instant::now() + Duration::from_micros(400);
        sleep.as_mut().reset(soon);
        assert_eq!(sleep.deadline(), soon);
        sleep.as_mut().await;
        assert!(Instant::now() >= soon);
        assert!(sleep.is_elapsed());
    }));
}

#[test]
fn many_precise_sleeps_at_once_all_end_after_their_deadlines() {
    let rt = runtime(2, false);
    rt.block_on(async {
        let tasks: Vec<_> = (0..200u64)
            .map(|i| {
                nuclei::spawn(async move {
                    let d = Duration::from_micros(50 + (i * 37) % 900);
                    let start = std::time::Instant::now();
                    time::sleep_precise(d).await;
                    start.elapsed() >= d
                })
            })
            .collect();
        for task in tasks {
            assert!(task.await, "a sleep ended early");
        }
    });
}

#[test]
fn a_dropped_precise_sleep_does_not_wake_its_task() {
    let rt = runtime(1, false);
    rt.block_on(rt.spawn(async {
        // Dropped armed: after a poll that found its deadline still ahead. A machine that
        // stalls past the deadline before that poll makes a fresh sleep, a bounded number of
        // times.
        let armed = (0..100).any(|_| {
            let mut sleep = Box::pin(time::sleep_precise(Duration::from_micros(200)));
            poll_now(sleep.as_mut()).is_pending()
        });
        assert!(
            armed,
            "no precise sleep of 200 microseconds was pending at its first poll"
        );
        let start = std::time::Instant::now();
        time::sleep_precise(Duration::from_millis(2)).await;
        assert!(start.elapsed() >= Duration::from_millis(2));
    }));
}

/// A waker that unparks the thread that made it.
struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` on the calling thread, which runs no nuclei shard.
fn block_on_plain<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::park();
    }
}

#[test]
fn a_precise_sleep_polled_off_a_shard_ends_after_its_deadline() {
    let _rt = runtime(1, false);
    let took = std::thread::spawn(|| {
        block_on_plain(async {
            let d = Duration::from_micros(600);
            let start = std::time::Instant::now();
            time::sleep_precise(d).await;
            start.elapsed() >= d
        })
    })
    .join()
    .unwrap();
    assert!(took, "ended early off a shard");
}

#[test]
fn a_precise_sleep_dropped_off_a_shard_leaves_the_runtime_usable() {
    let rt = runtime(1, false);
    std::thread::spawn(|| {
        let mut sleep = Box::pin(time::sleep_precise(Duration::from_secs(3600)));
        let waker = Waker::noop();
        assert!(
            sleep
                .as_mut()
                .poll(&mut Context::from_waker(waker))
                .is_pending()
        );
        drop(sleep);
    })
    .join()
    .unwrap();
    rt.block_on(rt.spawn(async {
        time::sleep_precise(Duration::from_micros(100)).await;
    }));
}

/// Polls `future` once from inside the current task.
fn poll_now<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut future = future;
    let waker = Waker::noop();
    future.as_mut().poll(&mut Context::from_waker(waker))
}
