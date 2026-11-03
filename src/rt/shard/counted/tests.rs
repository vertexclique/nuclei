//! The counted queue: an empty look, items taken once and in order, and the count across
//! threads that push while the consumer takes.

use std::sync::Arc;

use super::Counted;

#[test]
fn an_empty_queue_says_so_and_a_push_is_seen() {
    let q = Counted::new();
    assert!(q.is_empty());
    let mut seen = Vec::new();
    q.take_all(|v: u32| seen.push(v));
    assert!(seen.is_empty(), "nothing to take");
    q.push(7);
    assert!(!q.is_empty());
    q.take_all(|v| seen.push(v));
    assert_eq!(seen, [7]);
    assert!(q.is_empty(), "the take took it off the count");
}

#[test]
fn a_take_takes_every_counted_item_once_and_in_order() {
    let q = Counted::new();
    for v in 0..100u32 {
        q.push(v);
    }
    let mut seen = Vec::new();
    q.take_all(|v| seen.push(v));
    assert_eq!(seen, (0..100).collect::<Vec<_>>());
    assert!(q.is_empty());
    q.take_all(|_| panic!("taken twice"));
}

#[test]
fn items_pushed_on_other_threads_are_all_taken_in_each_threads_order() {
    const THREADS: u32 = 4;
    const EACH: u32 = if cfg!(miri) { 200 } else { 2_000 };
    let q = Arc::new(Counted::new());
    let pushers: Vec<_> = (0..THREADS)
        .map(|t| {
            let q = Arc::clone(&q);
            std::thread::spawn(move || {
                for i in 0..EACH {
                    q.push((t, i));
                }
            })
        })
        .collect();
    let mut next = [0u32; THREADS as usize];
    let mut taken = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while taken < THREADS * EACH {
        assert!(
            std::time::Instant::now() < deadline,
            "{taken} of {} taken; the rest never came off the queue",
            THREADS * EACH
        );
        if q.is_empty() {
            std::thread::yield_now();
            continue;
        }
        q.take_all(|(t, i)| {
            assert_eq!(i, next[t as usize], "thread {t}'s items out of order");
            next[t as usize] += 1;
            taken += 1;
        });
    }
    for pusher in pushers {
        pusher.join().expect("a pusher panicked");
    }
    assert!(q.is_empty(), "every count taken off");
    q.take_all(|_| panic!("an item left behind its count"));
}
