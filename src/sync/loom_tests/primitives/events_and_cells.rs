//! Events, latches, barriers and once-cells.

use super::*;

#[test]
fn an_event_set_concurrently_with_waits_wakes_them() {
    explore(
        "an_event_set_concurrently_with_waits_wakes_them",
        2,
        5,
        || {
            let event = Arc::new(Event::new());
            let waiters: Vec<_> = (0..2)
                .map(|_| {
                    let event = Arc::clone(&event);
                    thread::spawn(move || block_on(event.wait()))
                })
                .collect();
            event.set();
            for waiter in waiters {
                waiter.join().expect("waiter");
            }
        },
    );
}

#[test]
fn a_latch_opens_after_concurrent_count_downs() {
    explore("a_latch_opens_after_concurrent_count_downs", 2, 5, || {
        let latch = Arc::new(Latch::new(2));
        let counters: Vec<_> = (0..2)
            .map(|_| {
                let latch = Arc::clone(&latch);
                thread::spawn(move || latch.count_down())
            })
            .collect();
        block_on(latch.wait());
        for counter in counters {
            counter.join().expect("counter");
        }
        assert_eq!(latch.count(), 0);
    });
}

#[test]
fn a_barrier_releases_both_with_one_leader() {
    explore("a_barrier_releases_both_with_one_leader", 3, 5, || {
        let barrier = Arc::new(Barrier::new(2));
        let other = {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || block_on(barrier.wait()).is_leader())
        };
        let mine = block_on(barrier.wait()).is_leader();
        let theirs = other.join().expect("other");
        assert!(mine ^ theirs, "exactly one leader");
    });
}

#[test]
fn once_cell_initializers_agree_on_one_value() {
    explore("once_cell_initializers_agree_on_one_value", 3, 5, || {
        let cell = Arc::new(OnceCell::new());
        let racer = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || *cell.get_or_init_racy(|| 1))
        };
        let mine = *block_on(cell.get_or_init(|| async { 2 }));
        assert_eq!(racer.join().expect("racer"), mine);
    });
}

#[test]
fn a_cancelled_once_cell_initializer_hands_the_turn_on() {
    explore(
        "a_cancelled_once_cell_initializer_hands_the_turn_on",
        3,
        5,
        || {
            let cell = Arc::new(OnceCell::new());
            let quitter = {
                let cell = Arc::clone(&cell);
                thread::spawn(move || poll_once_then_drop(cell.get_or_init(std::future::pending)))
            };
            assert_eq!(*block_on(cell.get_or_init(|| async { 2 })), 2);
            quitter.join().expect("quitter");
        },
    );
}

#[test]
fn a_racing_publish_wakes_a_waiter_queued_behind_a_running_initializer() {
    explore(
        "a_racing_publish_wakes_a_waiter_queued_behind_a_running_initializer",
        3,
        5,
        || {
            let cell = Arc::new(OnceCell::new());
            let mut stuck = Box::pin(cell.get_or_init(std::future::pending));
            assert!(
                stuck
                    .as_mut()
                    .poll(&mut Context::from_waker(std::task::Waker::noop()))
                    .is_pending()
            );
            let waiter = {
                let cell = Arc::clone(&cell);
                thread::spawn(move || *block_on(cell.get_or_init(|| async { 2 })))
            };
            assert_eq!(*cell.get_or_init_racy(|| 1), 1);
            assert_eq!(waiter.join().expect("waiter"), 1);
            drop(stuck);
        },
    );
}

#[test]
fn a_dropped_barrier_wait_stays_counted_while_its_generation_completes() {
    explore(
        "a_dropped_barrier_wait_stays_counted_while_its_generation_completes",
        3,
        5,
        || {
            let barrier = Arc::new(Barrier::new(2));
            let mut first = Box::pin(barrier.wait());
            assert!(
                first
                    .as_mut()
                    .poll(&mut Context::from_waker(std::task::Waker::noop()))
                    .is_pending()
            );
            let second = {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || block_on(barrier.wait()).is_leader())
            };
            drop(first);
            assert!(
                second.join().expect("second"),
                "the dropped arrival still counts, so the second completes the pair"
            );
            // The next generation starts clean.
            let third = {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || block_on(barrier.wait()).is_leader())
            };
            let fourth = block_on(barrier.wait()).is_leader();
            assert!(third.join().expect("third") ^ fourth, "exactly one leader");
        },
    );
}

#[test]
fn a_cancelled_event_wait_racing_the_set_strands_nobody() {
    explore(
        "a_cancelled_event_wait_racing_the_set_strands_nobody",
        2,
        5,
        || {
            let event = Arc::new(Event::new());
            let canceller = {
                let event = Arc::clone(&event);
                thread::spawn(move || poll_once_then_drop(event.wait()))
            };
            let waiter = {
                let event = Arc::clone(&event);
                thread::spawn(move || block_on(event.wait()))
            };
            event.set();
            canceller.join().expect("canceller");
            waiter.join().expect("waiter");
        },
    );
}
