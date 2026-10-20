//! The mutex: exclusion, no lost wakeup, the bounded bypass and a cancelled waiter.

use super::*;

contended!(
    mutex_excludes_and_loses_no_wakeup,
    mutex_excludes_and_loses_no_wakeup_at_bound_2,
    10,
    // SHRUNK: the main thread holds the lock from the start, where all three
    // threads used to race for it (462 thousand interleavings, 58 s). Two
    // threads still wait for one lock and each must be woken: the release
    // hands it to one, whose release hands it to the other, and a barging
    // waiter may take it between the two. What is left out is a third
    // thread parking: the holder never waits.
    || {
        let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
        let held = mutex.try_lock().expect("free");
        let others: Vec<_> = (0..2)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                thread::spawn(move || bump(&block_on(mutex.lock())))
            })
            .collect();
        bump(&held);
        drop(held);
        for other in others {
            other.join().expect("locker");
        }
        assert_eq!(read(&mutex.try_lock().expect("free at the end")), 3);
    },
    // The bound-2 twin: one thread waits and one only tries (`try_lock`), so
    // a single thread parks. What races at bound 2 is the release against the
    // waiter's arrival and against a caller barging in ahead of it.
    || {
        let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
        let held = mutex.try_lock().expect("free");
        let waiter = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || bump(&block_on(mutex.lock())))
        };
        let barger = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || mutex.try_lock().map(|guard| bump(&guard)).is_ok())
        };
        bump(&held);
        drop(held);
        waiter.join().expect("waiter");
        let barged = barger.join().expect("barger");
        assert_eq!(
            read(&mutex.try_lock().expect("free at the end")),
            2 + usize::from(barged)
        );
    }
);

#[test]
fn a_waiter_barged_past_max_bypass_times_is_handed_the_mutex() {
    explore(
        "a_waiter_barged_past_max_bypass_times_is_handed_the_mutex",
        2,
        10,
        || {
            let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
            let mut held = Some(mutex.try_lock().expect("free"));
            let queued = Arc::new(AtomicBool::new(false));
            let waiter = {
                let (mutex, flag) = (Arc::clone(&mutex), Arc::clone(&queued));
                thread::spawn(move || bump(&block_on(flag_when_queued(mutex.lock(), &flag))))
            };
            wait_for(&queued);
            // Barge back in after every release, once more than the bound
            // allows: past it the release must go to the waiter.
            let mut barged = 0;
            for _ in 0..=MAX_BYPASS {
                drop(held.take());
                held = mutex.try_lock().ok();
                if let Some(guard) = &held {
                    bump(guard);
                    barged += 1;
                }
            }
            drop(held);
            waiter.join().expect("waiter");
            assert_eq!(read(&mutex.try_lock().expect("free")), barged + 1);
        },
    );
}

contended!(
    a_cancelled_mutex_waiter_racing_the_handoff_loses_nothing,
    a_cancelled_mutex_waiter_racing_the_handoff_loses_nothing_at_bound_2,
    10,
    || {
        let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
        let held = mutex.try_lock().expect("free");
        let canceller = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || poll_once_then_drop(mutex.lock()))
        };
        let waiter = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || bump(&block_on(mutex.lock())))
        };
        drop(held);
        canceller.join().expect("canceller");
        waiter.join().expect("waiter");
        assert_eq!(read(&mutex.try_lock().expect("the lock was not leaked")), 1);
    },
    // The bound-2 twin: the canceller is queued before the waiter exists, so
    // the order of the two arrivals is fixed and only the cancellation races
    // the waiter's arrival and the release. That is the case the model is
    // for: a handoff given to a waiter that is going away must reach the one
    // behind it.
    || {
        let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
        let held = mutex.try_lock().expect("free");
        let queued = Arc::new(AtomicBool::new(false));
        let canceller = {
            let (mutex, flag) = (Arc::clone(&mutex), Arc::clone(&queued));
            thread::spawn(move || poll_once_then_drop(flag_when_queued(mutex.lock(), &flag)))
        };
        wait_for(&queued);
        let waiter = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || bump(&block_on(mutex.lock())))
        };
        drop(held);
        canceller.join().expect("canceller");
        waiter.join().expect("waiter");
        assert_eq!(read(&mutex.try_lock().expect("the lock was not leaked")), 1);
    }
);

/// A queued waiter does not keep the lock from a caller that finds it
/// free: barging is the design, so expecting the queue to win must fail.
#[test]
#[should_panic(expected = "a release went to a caller ahead of the queued waiter")]
fn calibration_a_queued_waiter_can_be_barged_past() {
    explore(
        "calibration_a_queued_waiter_can_be_barged_past",
        3,
        1,
        || {
            let mutex = Arc::new(Mutex::new(()));
            let held = mutex.try_lock().expect("free");
            let queued = Arc::new(AtomicBool::new(false));
            let waiter = {
                let (mutex, flag) = (Arc::clone(&mutex), Arc::clone(&queued));
                thread::spawn(move || drop(block_on(flag_when_queued(mutex.lock(), &flag))))
            };
            wait_for(&queued);
            drop(held);
            let barged = mutex.try_lock().is_ok();
            waiter.join().expect("waiter");
            assert!(
                !barged,
                "a release went to a caller ahead of the queued waiter"
            );
        },
    );
}
