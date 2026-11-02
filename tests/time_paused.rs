//! Paused time: a paused clock stands still, moves by `advance`, and jumps by
//! itself to the earliest timer when every shard of its runtime is idle and
//! no blocking job runs; on one shard or several.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Real time, for the bounds that show a paused sleep did not really wait.
fn real() -> std::time::Instant {
    std::time::Instant::now()
}

#[test]
fn a_sleep_on_a_paused_clock_ends_at_once_at_its_deadline() {
    let rt = runtime(0, true);
    let began = real();
    rt.block_on(async {
        let start = Instant::now();
        time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(Instant::now() - start, Duration::from_secs(3600));
    });
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "the hour was not skipped"
    );
}

#[test]
fn a_paused_clock_stands_still_until_advanced() {
    let rt = runtime(0, false);
    rt.block_on(async {
        time::pause();
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(Instant::now(), start);
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        // The deadline is set here, when the sleep is made, before any advance.
        let sleep = time::sleep(Duration::from_millis(100));
        let sleeper = nuclei::spawn_local(async move {
            sleep.await;
            flag.store(true, Ordering::SeqCst);
        });
        time::advance(Duration::from_millis(50)).await;
        assert!(!fired.load(Ordering::SeqCst), "fired 50 ms early");
        assert_eq!(Instant::now() - start, Duration::from_millis(50));
        time::advance(Duration::from_millis(60)).await;
        sleeper.await;
        assert!(fired.load(Ordering::SeqCst));
        assert_eq!(Instant::now() - start, Duration::from_millis(110));
    });
}

#[test]
fn timers_fire_in_deadline_order_when_the_clock_jumps() {
    let rt = runtime(0, true);
    rt.block_on(async {
        let start = Instant::now();
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tasks: Vec<_> = [300u64, 100, 500, 200, 400]
            .into_iter()
            .map(|ms| {
                let order = Arc::clone(&order);
                nuclei::spawn_local(async move {
                    time::sleep(Duration::from_millis(ms)).await;
                    order.lock().unwrap().push((ms, Instant::now() - start));
                })
            })
            .collect();
        for task in tasks {
            task.await;
        }
        let order = order.lock().unwrap().clone();
        assert_eq!(
            order.iter().map(|(ms, _)| *ms).collect::<Vec<_>>(),
            [100, 200, 300, 400, 500]
        );
        for (ms, at) in order {
            assert_eq!(
                at,
                Duration::from_millis(ms),
                "woke at its deadline exactly"
            );
        }
    });
}

#[test]
fn an_interval_and_a_timeout_run_on_paused_time() {
    let rt = runtime(0, true);
    let began = real();
    rt.block_on(async {
        let start = Instant::now();
        let mut every = time::interval(Duration::from_secs(60));
        for _ in 0..5 {
            every.tick().await;
        }
        assert_eq!(Instant::now() - start, Duration::from_secs(240));
        let late = time::timeout(Duration::from_secs(3600), std::future::pending::<()>()).await;
        assert!(late.is_err());
        assert_eq!(Instant::now() - start, Duration::from_secs(240 + 3600));
    });
    assert!(began.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_resumed_clock_runs_on_from_where_it_stood() {
    let rt = runtime(0, true);
    rt.block_on(async {
        let start = Instant::now();
        time::advance(Duration::from_secs(5)).await;
        time::resume();
        let resumed = Instant::now();
        assert!(resumed - start >= Duration::from_secs(5));
        assert!(resumed - start < Duration::from_secs(6));
        let real_start = real();
        time::sleep(Duration::from_millis(30)).await;
        assert!(
            real_start.elapsed() >= Duration::from_millis(29),
            "a running clock waits for real"
        );
        // Paused again: back to jumps.
        time::pause();
        let paused = Instant::now();
        time::sleep(Duration::from_secs(600)).await;
        assert_eq!(Instant::now() - paused, Duration::from_secs(600));
    });
}

#[test]
fn a_blocking_job_in_flight_holds_the_clock() {
    let rt = runtime(0, true);
    rt.block_on(async {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let job = nuclei::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::SeqCst);
        });
        let seen = Arc::clone(&done);
        let sleeper = nuclei::spawn_local(async move {
            time::sleep(Duration::from_secs(10)).await;
            seen.load(Ordering::SeqCst)
        });
        job.await;
        assert!(
            sleeper.await,
            "the clock jumped while a blocking job could still wake a task"
        );
    });
}

#[test]
fn the_clock_jumps_over_every_shard_of_a_runtime_with_workers() {
    let rt = runtime(3, true);
    let began = real();
    rt.block_on(async {
        let start = Instant::now();
        let tasks: Vec<_> = (1..=12u64)
            .map(|i| {
                nuclei::spawn(async move {
                    for _ in 0..3 {
                        time::sleep(Duration::from_secs(i)).await;
                    }
                    Instant::now()
                })
            })
            .collect();
        let mut latest = start;
        for task in tasks {
            latest = latest.max(task.await);
        }
        // Every task slept 3 i seconds of paused time; the last ends at 36 s.
        assert!(latest - start >= Duration::from_secs(36));
        assert!(
            latest - start < Duration::from_secs(37),
            "{:?}",
            latest - start
        );
    });
    assert!(
        began.elapsed() < Duration::from_secs(20),
        "the time was not skipped"
    );
}

#[test]
fn a_runtime_whose_clock_never_paused_keeps_the_system_clock() {
    let rt = runtime(1, false);
    rt.block_on(async {
        let a = std::time::Instant::now();
        let b = Instant::now().into_std();
        let c = std::time::Instant::now();
        assert!(a <= b && b <= c);
        let start = real();
        time::sleep(Duration::from_millis(20)).await;
        assert!(start.elapsed() >= Duration::from_millis(19));
    });
}

#[test]
fn the_tasks_an_advance_makes_due_run_before_it_returns() {
    let rt = runtime(0, true);
    rt.block_on(async {
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        for ms in [30u64, 10, 20] {
            let order = Arc::clone(&order);
            let sleep = time::sleep(Duration::from_millis(ms));
            nuclei::spawn_local(async move {
                sleep.await;
                order.lock().unwrap().push(ms);
            })
            .detach();
        }
        // The tasks reach their sleeps before the clock moves.
        nuclei::task::yield_now().await;
        time::advance(Duration::from_millis(25)).await;
        assert_eq!(
            *order.lock().unwrap(),
            [10, 20],
            "due timers' tasks ran, in deadline order"
        );
        time::advance(Duration::from_millis(5)).await;
        assert_eq!(*order.lock().unwrap(), [10, 20, 30]);
    });
}

/// How long a test waits for what must happen at once, before it fails.
const SAFETY: Duration = Duration::from_secs(20);

/// Whether `done` came true within `within` of real time, looked at again
/// after a pause that doubles up to 5 ms.
fn within(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = real() + within;
    let mut pause = Duration::from_micros(10);
    while !done() {
        if real() >= deadline {
            return false;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
    true
}

/// `rt.block_on(future)` on a thread of its own, its output if it came
/// within [`SAFETY`].
fn block_on_within<F>(rt: &Runtime, future: F) -> Option<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = rt.handle();
    let (done, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(handle.block_on(future));
    });
    ended.recv_timeout(SAFETY).ok()
}

/// A paused runtime of one worker whose poll the handoff takes after 10 ms.
fn handed_on() -> Runtime {
    Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .start_paused(true)
        .build()
        .expect("the runtime starts")
}

/// Holds the poll that calls it until `open`, which only the new runner of
/// the poll's shard can set while the poll holds.
fn hold_until_handed_on(open: &AtomicBool) {
    assert!(
        within(SAFETY, || open.load(Ordering::SeqCst)),
        "the shard was never handed on"
    );
}

#[test]
fn a_local_task_left_on_a_thread_whose_shard_was_handed_on_sleeps_on_paused_time() {
    let rt = handed_on();
    let open = Arc::new(AtomicBool::new(false));
    let gate = Arc::clone(&open);
    let task = rt.spawn(async move {
        let start = Instant::now();
        // Bound to this thread; it first runs once the thread's shard went.
        let local = nuclei::spawn_local(async move {
            time::sleep(Duration::from_secs(3600)).await;
            Instant::now() - start
        });
        hold_until_handed_on(&gate);
        local.await
    });
    rt.block_on(rt.spawn(async {}));
    open.store(true, Ordering::SeqCst);
    assert_eq!(
        block_on_within(&rt, task),
        Some(Duration::from_secs(3600)),
        "the local task's sleep never ended: the shard it runs on is not counted by the paused clock"
    );
}

#[test]
fn a_paused_clock_stands_still_while_a_poll_the_handoff_took_still_runs() {
    let rt = handed_on();
    let poll = rt.spawn(async move {
        let start = Instant::now();
        let (armed, slept) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (is_armed, has_slept) = (Arc::clone(&armed), Arc::clone(&slept));
        // Queued behind this poll on its shard: it runs, and arms its sleep,
        // only once the shard was taken from this thread and runs on another.
        let sleeper = nuclei::spawn(async move {
            let sleep = time::sleep(Duration::from_secs(1));
            is_armed.store(true, Ordering::SeqCst);
            sleep.await;
            has_slept.store(true, Ordering::SeqCst);
        });
        hold_until_handed_on(&armed);
        // The shard's new runner parks with the sleep armed: a jump now
        // would come while this poll still runs.
        let jumped = within(Duration::from_secs(2), || slept.load(Ordering::SeqCst));
        (jumped, Instant::now() - start, sleeper)
    });
    let (jumped, elapsed, sleeper) = block_on_within(&rt, poll).expect("the poll ended");
    assert!(
        !jumped,
        "the paused clock jumped while a poll the handoff took still ran"
    );
    assert_eq!(elapsed, Duration::ZERO, "the clock moved under the poll");
    assert!(
        block_on_within(&rt, sleeper).is_some(),
        "the clock never jumped once the poll ended"
    );
}

#[test]
fn a_block_on_in_a_poll_the_handoff_took_sleeps_on_paused_time() {
    let rt = handed_on();
    let poll = rt.spawn(async move {
        let on_new = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&on_new);
        // Queued behind this poll: it runs only on the shard's new runner.
        nuclei::spawn(async move { ran.store(true, Ordering::SeqCst) }).detach();
        hold_until_handed_on(&on_new);
        // Taken from its shard, the poll's `block_on` runs on the thread's
        // own shard, a guest of the paused clock.
        let start = Instant::now();
        nuclei::block_on(time::sleep(Duration::from_secs(3600)));
        Instant::now() - start
    });
    assert_eq!(
        block_on_within(&rt, poll),
        Some(Duration::from_secs(3600)),
        "the sleep of a block_on in a poll the handoff took never ended"
    );
}

#[test]
fn a_nested_block_on_whose_shard_the_handoff_took_sleeps_on_paused_time() {
    let rt = handed_on();
    let poll = rt.spawn(async move {
        let on_new = Arc::new(AtomicBool::new(false));
        let ran = Arc::clone(&on_new);
        nuclei::spawn(async move { ran.store(true, Ordering::SeqCst) }).detach();
        // Nested on the worker's shard, which is taken during its poll: the
        // rest of the call runs on the thread's own shard, a guest.
        nuclei::block_on(async move {
            hold_until_handed_on(&on_new);
            let start = Instant::now();
            time::sleep(Duration::from_secs(3600)).await;
            Instant::now() - start
        })
    });
    assert_eq!(
        block_on_within(&rt, poll),
        Some(Duration::from_secs(3600)),
        "the sleep of a nested block_on whose shard was taken never ended"
    );
}
