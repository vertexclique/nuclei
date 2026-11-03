#![allow(unsafe_code)]

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

/// A waker whose first drop after `arm` panics: user code that fails as the
/// runtime lets go of a waker.
pub(crate) struct Bomb {
    armed: std::sync::atomic::AtomicBool,
}

impl Bomb {
    pub(crate) fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

static BOMB: core::task::RawWakerVTable =
    core::task::RawWakerVTable::new(bomb_clone, bomb_drop, bomb_wake_by_ref, bomb_drop);

unsafe fn bomb_clone(data: *const ()) -> core::task::RawWaker {
    // SAFETY: `data` came from `Arc::into_raw` of a `Bomb` and is live.
    unsafe { std::sync::Arc::increment_strong_count(data.cast::<Bomb>()) };
    core::task::RawWaker::new(data, &BOMB)
}

unsafe fn bomb_wake_by_ref(_: *const ()) {}

#[allow(clippy::panic)]
unsafe fn bomb_drop(data: *const ()) {
    // SAFETY: consumes the count this waker owns.
    let bomb = unsafe { std::sync::Arc::from_raw(data.cast::<Bomb>()) };
    if bomb.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
        panic!("a waker panicked as it was dropped");
    }
}

pub(crate) fn bomb() -> (std::sync::Arc<Bomb>, core::task::Waker) {
    let bomb = std::sync::Arc::new(Bomb {
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let data = std::sync::Arc::into_raw(std::sync::Arc::clone(&bomb)).cast::<()>();
    // SAFETY: the vtable's contract is met by a pointer from `Arc::into_raw`.
    let waker = unsafe { core::task::Waker::from_raw(core::task::RawWaker::new(data, &BOMB)) };
    (bomb, waker)
}

fn boom() -> core::task::Waker {
    crate::sync::test_support::hooked(
        None,
        Some(Box::new(|| panic!("a waker panicked as it was woken"))),
    )
}

fn paused(workers: usize) -> crate::runtime::Runtime {
    crate::runtime::Builder::new()
        .worker_threads(workers)
        .pin_workers(false)
        .start_paused(true)
        .build()
        .unwrap()
}

#[test]
fn a_waker_that_panics_as_its_timer_fires_leaves_no_other_fired_timer_unwoken() {
    use crate::sync::test_support::Polled;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let rt = paused(0);
    rt.block_on(async {
        // Same slot, so the three fire in one go; the middle one panics, so
        // one timer is behind it whichever way the slot is read.
        let mut before = Polled::new(sleep(Duration::from_millis(10)));
        before.pending();
        let mut middle = Box::pin(sleep(Duration::from_millis(10)));
        let waker = boom();
        let mut cx = core::task::Context::from_waker(&waker);
        assert!(middle.as_mut().poll(&mut cx).is_pending());
        let mut after = Polled::new(sleep(Duration::from_millis(10)));
        after.pending();

        let advanced = catch_unwind(AssertUnwindSafe(|| {
            crate::rt::paused::advance(Duration::from_millis(11));
        }));
        assert!(advanced.is_err(), "the panic reaches the one who advanced");
        assert_eq!(before.wakes(), 1);
        assert_eq!(after.wakes(), 1);
        before.ready();
        after.ready();
    });
}

#[test]
fn a_waker_that_panics_as_the_run_loop_fires_it_does_not_kill_the_worker() {
    let rt = paused(1);
    let (done, ended) = std::sync::mpsc::channel();
    let _task = rt.spawn(async move {
        let mut first = Box::pin(sleep(Duration::from_secs(1)));
        {
            let waker = boom();
            let mut cx = core::task::Context::from_waker(&waker);
            assert!(first.as_mut().poll(&mut cx).is_pending());
        }
        sleep(Duration::from_secs(2)).await;
        let _ = done.send(());
    });
    ended
        .recv_timeout(Duration::from_secs(20))
        .expect("the worker kept serving after a timer's waker panicked");
}

#[test]
fn a_timer_panic_leaving_a_nested_block_on_leaves_no_mark_behind() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        let mut s = Box::pin(sleep(Duration::from_millis(1)));
        let waker = boom();
        let mut cx = core::task::Context::from_waker(&waker);
        assert!(s.as_mut().poll(&mut cx).is_pending());
        let nested = catch_unwind(AssertUnwindSafe(|| {
            shard.block_on(sleep(Duration::from_millis(30)));
        }));
        assert!(
            nested.is_err(),
            "the timer's panic leaves the nested block_on"
        );
        assert!(!crate::rt::shard::fire_panic_set());
    });
}

#[test]
fn a_waker_dropped_as_a_removal_applies_does_not_lose_the_ops_after_it() {
    let shard = shard();
    let _current = context::enter(&shard);
    let (bomb, waker) = bomb();
    let sleeps = shard.block_on(async {
        let mut sleeps = Vec::new();
        for _ in 0..3 {
            let mut s = Box::pin(sleep(Duration::from_secs(3600)));
            let mut cx = core::task::Context::from_waker(&waker);
            assert!(s.as_mut().poll(&mut cx).is_pending());
            sleeps.push(s);
        }
        sleeps
    });
    assert_eq!(shard.wheel().borrow().armed(), 3);
    bomb.arm();
    std::thread::spawn(move || drop(sleeps)).join().unwrap();
    // The panic of the dropped waker reaches the loop's caller, after every
    // removal was applied.
    let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shard.block_on(crate::task::yield_now());
    }));
    assert!(turn.is_err());
    assert_eq!(shard.wheel().borrow().armed(), 0);
}

#[test]
fn a_clone_that_panics_mid_arm_leaves_nothing_half_armed() {
    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        let mut s = Box::pin(sleep(Duration::from_secs(3600)));
        let waker = crate::sync::test_support::hooked(
            Some(Box::new(|| panic!("a waker panicked as it was cloned"))),
            None,
        );
        let mut cx = core::task::Context::from_waker(&waker);
        let polled =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.as_mut().poll(&mut cx)));
        assert!(polled.is_err());
        assert_eq!(shard.wheel().borrow().armed(), 0);
        // The sleep arms normally on its next poll.
        let mut again = core::task::Context::from_waker(core::task::Waker::noop());
        assert!(s.as_mut().poll(&mut again).is_pending());
        assert_eq!(shard.wheel().borrow().armed(), 1);
    });
}

#[test]
fn a_clone_that_arms_a_sleep_on_the_same_shard_does_not_meet_the_wheels_borrow() {
    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        let other = std::rc::Rc::new(core::cell::RefCell::new(None));
        let arming = std::rc::Rc::clone(&other);
        let hook = move || {
            let mut s = Box::pin(sleep(Duration::from_secs(3600)));
            let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
            assert!(s.as_mut().poll(&mut cx).is_pending());
            *arming.borrow_mut() = Some(s);
        };
        let mut s = Box::pin(sleep(Duration::from_secs(3600)));
        let waker = crate::sync::test_support::hooked(Some(Box::new(hook)), None);
        let mut cx = core::task::Context::from_waker(&waker);
        assert!(s.as_mut().poll(&mut cx).is_pending());
        assert_eq!(shard.wheel().borrow().armed(), 2);

        // The same on the poll that replaces the waker.
        let arming = std::rc::Rc::clone(&other);
        let hook = move || {
            let mut s = Box::pin(sleep(Duration::from_secs(3600)));
            let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
            assert!(s.as_mut().poll(&mut cx).is_pending());
            *arming.borrow_mut() = Some(s);
        };
        let waker = crate::sync::test_support::hooked(Some(Box::new(hook)), None);
        let mut cx = core::task::Context::from_waker(&waker);
        assert!(s.as_mut().poll(&mut cx).is_pending());
        assert_eq!(shard.wheel().borrow().armed(), 2);
    });
}

#[test]
fn a_refused_poll_of_a_due_sleep_answers_ready_at_the_next_poll() {
    use crate::sync::test_support::with_budget_spent;

    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        // Due at once: its deadline is not after now.
        let mut due = sleep(Duration::ZERO);
        let refused = core::future::poll_fn(|cx| {
            Poll::Ready(with_budget_spent(|| Pin::new(&mut due).poll(cx)))
        })
        .await;
        assert!(
            refused.is_pending(),
            "a spent budget refuses the completion"
        );
        assert!(
            poll_once(&mut due).await.is_ready(),
            "and the timer stayed done"
        );

        // Fired in the wheel: the same.
        let mut armed = sleep(Duration::from_millis(1));
        assert!(poll_once(&mut armed).await.is_pending());
        sleep(Duration::from_millis(5)).await;
        let refused = core::future::poll_fn(|cx| {
            Poll::Ready(with_budget_spent(|| Pin::new(&mut armed).poll(cx)))
        })
        .await;
        assert!(refused.is_pending());
        assert!(poll_once(&mut armed).await.is_ready());
        assert_eq!(shard.wheel().borrow().armed(), 0);
    });
}

#[test]
fn a_pending_poll_spends_no_budget() {
    let shard = shard();
    let _current = context::enter(&shard);
    shard.block_on(async {
        let mut far = sleep(Duration::from_secs(3600));
        let left = core::future::poll_fn(|cx| {
            let before = crate::coop::has_budget_remaining();
            let polled = Pin::new(&mut far).poll(cx);
            Poll::Ready((
                before,
                polled.is_pending(),
                crate::coop::has_budget_remaining(),
            ))
        })
        .await;
        assert_eq!(left, (true, true, true));
    });
}
