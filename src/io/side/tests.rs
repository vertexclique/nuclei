//! The contended set: more tasks than slots, and a slot being taken.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Wake, Waker};

use super::{After, Side, Unwound, WAITING, WakerSet};

struct Count(AtomicUsize);

impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counted() -> (Arc<Count>, Waker) {
    let count = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&count));
    (count, waker)
}

/// Six tasks in a set of four: each is woken by the take, or at once when
/// a later one displaced it from the shared slot; none is left unwoken.
#[test]
fn every_task_is_woken_even_past_the_slots() {
    let set = WakerSet::<4>::new();
    let tasks: Vec<_> = (0..6).map(|_| counted()).collect();
    for (_, waker) in &tasks {
        assert!(set.register(waker));
    }
    let mut after = After::default();
    set.take_all(&mut after);
    after.run();
    for (i, (count, _)) in tasks.iter().enumerate() {
        assert!(
            count.0.load(Ordering::SeqCst) >= 1,
            "task {i} was never woken"
        );
    }
}

/// A task that registers again keeps one slot: a take wakes it once.
#[test]
fn a_task_registering_again_keeps_its_slot() {
    let set = WakerSet::<4>::new();
    let (count, waker) = counted();
    for _ in 0..10 {
        assert!(set.register(&waker));
    }
    let mut after = After::default();
    set.take_all(&mut after);
    after.run();
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    // The slots are free again: another take wakes nobody.
    let mut after = After::default();
    set.take_all(&mut after);
    after.run();
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
}

/// A side with one waker in its contended set and `WAITING` raised, so a
/// guard's drop wakes it.
fn contended_side() -> (Arc<Count>, Side<()>) {
    let side = Side::new(());
    let (count, waker) = counted();
    assert!(side.contended().register(&waker));
    side.state.fetch_or(WAITING, Ordering::SeqCst);
    (count, side)
}

/// A guard dropped while not unwinding wakes the contended polls at once.
#[test]
fn a_guard_dropped_normally_wakes_at_once() {
    let (count, side) = contended_side();
    drop(side.try_hold().expect("the side is free"));
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
}

/// A guard dropped by unwinding parks its wakes; the guard declared before
/// the view runs them once the view is gone, and not before.
#[test]
fn a_guard_dropped_by_unwinding_parks_its_wakes_for_the_draining_guard() {
    use std::cell::Cell;

    /// Records the wake count when it drops: the view's place.
    struct View<'a>(&'a Count, &'a Cell<Option<usize>>);
    impl Drop for View<'_> {
        fn drop(&mut self) {
            self.1.set(Some(self.0.0.load(Ordering::SeqCst)));
        }
    }

    let (count, side) = contended_side();
    let at_view_drop = Cell::new(None);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _unwound = Unwound::enter();
        let _view = View(&count, &at_view_drop);
        let _held = side.try_hold().expect("the side is free");
        panic!("a panic under the guard");
    }));
    assert!(unwound.is_err());
    assert_eq!(at_view_drop.get(), Some(0), "woken while the view was live");
    assert_eq!(
        count.0.load(Ordering::SeqCst),
        1,
        "the draining guard ran the wake"
    );
}

/// A panic under a guard outside a live view parks nothing: the wake runs
/// during unwinding, as before.
#[test]
fn unwinding_outside_a_view_wakes_at_once() {
    let (count, side) = contended_side();
    let at_catch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = side.try_hold().expect("the side is free");
        panic!("a panic under the guard, no view");
    }));
    assert!(at_catch.is_err());
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    super::run_unwound();
    assert_eq!(count.0.load(Ordering::SeqCst), 1, "nothing was parked");
}
