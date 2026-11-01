//! The counted queue made at its first push: nothing made by a look or a take, the first push
//! makes it, a race to make it keeps one queue, and what it holds goes with it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::LazyCounted;

#[test]
fn a_look_and_a_take_make_nothing_and_the_first_push_makes_the_queue() {
    let q = LazyCounted::new();
    assert!(q.is_empty());
    q.take_all(|_: u32| panic!("nothing was pushed"));
    assert!(q.made().is_none(), "a look or a take made the queue");
    q.push(1);
    assert!(q.made().is_some(), "the push made no queue");
    assert!(!q.is_empty());
    q.push(2);
    let mut seen = Vec::new();
    q.take_all(|v| seen.push(v));
    assert_eq!(seen, [1, 2]);
    assert!(q.is_empty(), "the take took them off the count");
}

#[test]
fn pushers_that_race_to_make_the_queue_share_one_and_lose_nothing() {
    const THREADS: u32 = 8;
    const EACH: u32 = 500;
    for _ in 0..20 {
        let q = Arc::new(LazyCounted::new());
        let start = Arc::new(std::sync::Barrier::new(THREADS as usize));
        let pushers: Vec<_> = (0..THREADS)
            .map(|t| {
                let (q, start) = (Arc::clone(&q), Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    for i in 0..EACH {
                        q.push((t, i));
                    }
                })
            })
            .collect();
        for pusher in pushers {
            pusher.join().unwrap();
        }
        let mut next = [0u32; THREADS as usize];
        q.take_all(|(t, i)| {
            assert_eq!(i, next[t as usize], "thread {t}'s items out of order");
            next[t as usize] += 1;
        });
        assert!(next.iter().all(|&n| n == EACH), "items lost: {next:?}");
    }
}

#[test]
fn what_the_queue_holds_is_dropped_with_it() {
    struct Counts(Arc<AtomicUsize>);
    impl Drop for Counts {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    let q = LazyCounted::new();
    for _ in 0..40 {
        q.push(Counts(Arc::clone(&dropped)));
    }
    drop(q);
    assert_eq!(dropped.load(Ordering::Relaxed), 40);
    drop(LazyCounted::<Counts>::new());
}
