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
    // The removal waits in the shard's mailbox until its next look, which a
    // yield's turn of the loop makes. A sleep would not: one whose first poll
    // comes after its deadline ends at once, with no turn.
    shard.block_on(crate::task::yield_now());
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
    // As for the millisecond wheel: a yield's turn applies the removal.
    shard.block_on(crate::task::yield_now());
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
    let at =
        crate::time::clock::nanos(crate::time::clock::origin() + Duration::from_nanos(2_000_001));
    assert_eq!(shard.time_of(true, at), 2_001);
    assert_eq!(shard.time_of(false, at), 3);
    let exact = crate::time::clock::nanos(crate::time::clock::origin() + Duration::from_micros(7));
    assert_eq!(shard.time_of(true, exact), 7);
}

/// A waker that counts its wakes.
struct Counting(std::sync::atomic::AtomicUsize);

impl std::task::Wake for Counting {
    fn wake(self: std::sync::Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn counting() -> (std::sync::Arc<Counting>, core::task::Waker) {
    let count = std::sync::Arc::new(Counting(std::sync::atomic::AtomicUsize::new(0)));
    let waker = core::task::Waker::from(std::sync::Arc::clone(&count));
    (count, waker)
}

fn wakes(count: &Counting) -> usize {
    count.0.load(std::sync::atomic::Ordering::SeqCst)
}

/// Waits for `done` within ten seconds, looking again after a pause that
/// doubles up to 5 ms.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(10);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "{what} never happened"
        );
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

#[test]
fn a_sleep_moved_to_another_task_as_its_shard_goes_is_armed_again() {
    let rt = crate::runtime::Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .unwrap();
    // Long enough that the first poll comes before the deadline on a loaded
    // machine, short enough that the timer armed again fires soon.
    let mut s = sleep(Duration::from_secs(1));
    let (first, first_waker) = counting();
    // Polled on a thread that runs no shard: one of `rt`'s workers holds it.
    {
        let _inside = rt.enter();
        let mut cx = core::task::Context::from_waker(&first_waker);
        assert!(Pin::new(&mut s).poll(&mut cx).is_pending());
    }
    // The sleep moves to another task. Its poll finds the cell neither fired
    // nor orphaned, and before it registers its waker the shard goes: the
    // cell is orphaned, and the first task's waker, the one it held, woken.
    let (second, second_waker) = counting();
    let gone = std::sync::Arc::clone(&first);
    super::sleep::BEFORE_REGISTER.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            drop(rt);
            eventually("the shard's going woke the waker the cell held", || {
                wakes(&gone) == 1
            });
        }));
    });
    let mut cx = core::task::Context::from_waker(&second_waker);
    if Pin::new(&mut s).poll(&mut cx).is_pending() {
        eventually("the sleep's new task woken", || wakes(&second) > 0);
    }
    assert_eq!(wakes(&first), 1);
}
