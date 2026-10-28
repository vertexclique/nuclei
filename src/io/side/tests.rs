//! The contended set: more tasks than slots, and a slot being taken.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Wake, Waker};

use super::{After, WakerSet};

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
