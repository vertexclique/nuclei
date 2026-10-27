//! The semaphore: never more permits out than exist, and a large request is not starved.

use super::*;

contended!(
    a_semaphore_never_hands_out_more_permits_than_it_has,
    a_semaphore_never_hands_out_more_permits_than_it_has_at_bound_2,
    10,
    || {
        let sem = Arc::new(Semaphore::new(2));
        let out = Arc::new(AtomicUsize::new(0));
        let held = sem.try_acquire_many(2).expect("free");
        let takers: Vec<_> = [2, 1]
            .into_iter()
            .map(|n| {
                let (sem, out) = (Arc::clone(&sem), Arc::clone(&out));
                thread::spawn(move || {
                    let permit = block_on(sem.acquire_many(n as u32)).expect("open");
                    assert!(out.fetch_add(n, Ordering::SeqCst) + n <= 2, "overdrawn");
                    out.fetch_sub(n, Ordering::SeqCst);
                    drop(permit);
                })
            })
            .collect();
        drop(held);
        for taker in takers {
            taker.join().expect("taker");
        }
        assert_eq!(sem.available_permits(), 2);
    },
    // The bound-2 twin: the request for one permit only tries
    // (`try_acquire`), so a single thread parks. What races at bound 2 is
    // the release against the large request's arrival and against the small
    // caller taking a permit ahead of it.
    || {
        let sem = Arc::new(Semaphore::new(2));
        let out = Arc::new(AtomicUsize::new(0));
        let held = sem.try_acquire_many(2).expect("free");
        let big = {
            let (sem, out) = (Arc::clone(&sem), Arc::clone(&out));
            thread::spawn(move || {
                let permit = block_on(sem.acquire_many(2)).expect("open");
                assert!(out.fetch_add(2, Ordering::SeqCst) + 2 <= 2, "overdrawn");
                out.fetch_sub(2, Ordering::SeqCst);
                drop(permit);
            })
        };
        let small = {
            let (sem, out) = (Arc::clone(&sem), Arc::clone(&out));
            thread::spawn(move || {
                if let Ok(permit) = sem.try_acquire() {
                    assert!(out.fetch_add(1, Ordering::SeqCst) < 2, "overdrawn");
                    out.fetch_sub(1, Ordering::SeqCst);
                    drop(permit);
                }
            })
        };
        drop(held);
        big.join().expect("big");
        small.join().expect("small");
        assert_eq!(sem.available_permits(), 2);
    }
);

#[test]
fn a_large_request_behind_small_callers_is_owed_its_permits() {
    explore(
        "a_large_request_behind_small_callers_is_owed_its_permits",
        // SHRUNK: two preemptions where there were three (4.7 million
        // interleavings, over 5 minutes; now about 200 thousand). The
        // scenario is unchanged: the large request still meets a small
        // caller barging past it more often than the bound allows, and two
        // preemptions still split the release, the barge and the handoff.
        2,
        5,
        || {
            let sem = Arc::new(Semaphore::new(2));
            let mut small = sem.try_acquire().ok();
            let other = sem.try_acquire().expect("free");
            let queued = Arc::new(AtomicBool::new(false));
            let big = {
                let (sem, flag) = (Arc::clone(&sem), Arc::clone(&queued));
                thread::spawn(move || {
                    block_on(flag_when_queued(sem.acquire_many(2), &flag))
                        .expect("open")
                        .num_permits()
                })
            };
            wait_for(&queued);
            // One permit at a time comes free and a small caller takes it
            // whenever it may, once more than the bound allows.
            for _ in 0..=MAX_BYPASS {
                drop(small.take());
                small = sem.try_acquire().ok();
            }
            drop(small);
            drop(other);
            assert_eq!(big.join().expect("big"), 2);
            assert_eq!(sem.available_permits(), 2);
        },
    );
}
