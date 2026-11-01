//! A step's `After`: its slots start unwritten, and what it holds is woken or
//! dropped exactly once, whether it runs or drops.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Wake, Waker};

use super::{After, Wakers};
use crate::buf::Pool;

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

fn wakes(count: &Count) -> usize {
    count.0.load(Ordering::SeqCst)
}

#[test]
fn a_step_that_left_nothing_is_settled_with_no_work() {
    assert!(After::default().forget_if_nothing().is_ok());
}

#[test]
fn a_raised_flag_alone_is_left_work() {
    let mut after = After::default();
    assert!(!after.starved() && !after.must_register());
    after.set_starved();
    assert!(after.starved() && !after.must_register());
    let mut after = after
        .forget_if_nothing()
        .expect_err("a starved side is work");
    after.set_must_register();
    assert!(after.must_register());
    after.run();

    let mut after = After::default();
    after.set_must_register();
    let after = after
        .forget_if_nothing()
        .expect_err("a registration is work");
    after.run();
}

#[test]
fn every_waker_is_woken_once_by_run() {
    let tasks: Vec<_> = (0..3).map(|_| counted()).collect();
    let mut after = After::default();
    for (_, waker) in &tasks {
        after.wake_one(waker.clone());
    }
    after.run();
    for (i, (count, _)) in tasks.iter().enumerate() {
        assert_eq!(wakes(count), 1, "task {i}");
    }
}

#[test]
fn a_dropped_step_wakes_what_it_held() {
    let (first, a) = counted();
    let (second, b) = counted();
    let mut after = After::default();
    after.wake_one(a);
    drop(after);
    assert_eq!(wakes(&first), 1);

    let mut after = After::default();
    after.wake_one(b.clone());
    after.wake_one(b);
    drop(after);
    assert_eq!(wakes(&second), 2, "each waker handed in is woken");
}

#[test]
fn a_list_of_wakers_is_woken_once_each() {
    let (first, a) = counted();
    let (second, b) = counted();
    let mut wakers = Wakers::default();
    wakers.register(&a);
    wakers.register(&b);
    wakers.register(&a);
    let mut after = After::default();
    after.wake(&mut wakers);
    assert!(wakers.is_empty());
    after.run();
    assert_eq!((wakes(&first), wakes(&second)), (1, 1));
}

#[test]
fn buffers_and_forgotten_wakers_go_with_no_wake() {
    let pool = Pool::new(1 << 20);
    let (count, waker) = counted();
    let (woken, other) = counted();
    let mut after = After::default();
    after.drop_buf(pool.get(512).expect("a buffer"));
    after.drop_buf(pool.get(512).expect("a second buffer"));
    after.forget_one(waker.clone());
    let mut list = Wakers::default();
    list.register(&waker);
    after.forget(&mut list);
    after.wake_one(other);
    after.run();
    assert_eq!(wakes(&count), 0, "a forgotten waker is not woken");
    assert_eq!(wakes(&woken), 1);
    assert_eq!(
        Arc::strong_count(&count),
        2,
        "only the test's own wakers are left"
    );
}
