//! The semaphore: permits, the `try_*` forms, closing, and the permits' `merge` and `split`.

use super::*;

// === Semaphore ===

#[test]
fn a_semaphore_holds_at_most_its_documented_maximum() {
    assert_eq!(Semaphore::MAX_PERMITS, usize::MAX >> 7);
    let sem = Semaphore::new(Semaphore::MAX_PERMITS);
    assert_eq!(sem.available_permits(), Semaphore::MAX_PERMITS);
    static STATIC: Semaphore = Semaphore::const_new(3);
    assert_eq!(STATIC.available_permits(), 3);
    assert_eq!(
        format!("{:?}", Semaphore::new(2)),
        "Semaphore { available_permits: 2, closed: false }"
    );
}

#[test]
#[should_panic(expected = "at most Semaphore::MAX_PERMITS")]
fn more_permits_than_the_maximum_are_refused() {
    let _ = Semaphore::new(Semaphore::MAX_PERMITS + 1);
}

#[test]
#[should_panic(expected = "at most Semaphore::MAX_PERMITS")]
fn adding_past_the_maximum_is_refused() {
    Semaphore::new(Semaphore::MAX_PERMITS).add_permits(1);
}

#[test]
fn permits_are_added_forgotten_and_acquired() {
    let sem = Semaphore::new(2);
    sem.add_permits(3);
    assert_eq!(sem.available_permits(), 5);
    assert_eq!(sem.forget_permits(2), 2);
    assert_eq!(sem.forget_permits(100), 3, "only the free ones go");
    assert_eq!(sem.forget_permits(1), 0);
    sem.add_permits(0);
    assert_eq!(sem.available_permits(), 0);
    assert_eq!(sem.try_acquire().unwrap_err(), TryAcquireError::NoPermits);
}

#[test]
fn acquire_and_acquire_many_wait_for_their_permits() {
    let sem = Semaphore::new(0);
    let mut one = Polled::new(sem.acquire());
    let mut two = Polled::new(sem.acquire_many(2));
    one.pending();
    two.pending();
    sem.add_permits(1);
    assert_eq!((one.wakes(), two.wakes()), (1, 0));
    let first = one.ready().expect("open");
    assert_eq!(first.num_permits(), 1);
    sem.add_permits(2);
    assert_eq!(two.wakes(), 1, "the two added permits are enough for it");
    let both = two.ready().expect("open");
    assert_eq!(both.num_permits(), 2);
    drop(first);
    assert_eq!(sem.available_permits(), 1);
    assert_eq!(
        block_on(sem.acquire_many(0)).expect("open").num_permits(),
        0
    );
}

#[test]
fn the_try_forms_tell_a_shortage_from_a_close() {
    let sem = Arc::new(Semaphore::new(2));
    let one = sem.try_acquire().expect("free");
    let owned = Arc::clone(&sem).try_acquire_owned().expect("free");
    assert_eq!(sem.try_acquire().unwrap_err(), TryAcquireError::NoPermits);
    assert_eq!(
        sem.try_acquire_many(2).unwrap_err(),
        TryAcquireError::NoPermits
    );
    assert_eq!(
        Arc::clone(&sem).try_acquire_owned().unwrap_err(),
        TryAcquireError::NoPermits
    );
    assert_eq!(
        Arc::clone(&sem).try_acquire_many_owned(2).unwrap_err(),
        TryAcquireError::NoPermits
    );
    drop((one, owned));
    let both = Arc::clone(&sem).try_acquire_many_owned(2).expect("free");
    assert_eq!(both.num_permits(), 2);
    assert!(Arc::ptr_eq(both.semaphore(), &sem));
    sem.close();
    drop(both);
    assert_eq!(
        sem.available_permits(),
        2,
        "permits returned to a closed semaphore count"
    );
    assert_eq!(sem.try_acquire().unwrap_err(), TryAcquireError::Closed);
    assert_eq!(
        sem.try_acquire_many(1).unwrap_err(),
        TryAcquireError::Closed
    );
    assert_eq!(
        sem.try_acquire_many(0).unwrap_err(),
        TryAcquireError::Closed
    );
    assert_eq!(
        Arc::clone(&sem).try_acquire_owned().unwrap_err(),
        TryAcquireError::Closed
    );
    assert_eq!(
        Arc::clone(&sem).try_acquire_many_owned(1).unwrap_err(),
        TryAcquireError::Closed
    );
}

#[test]
fn closing_ends_the_queued_waits_with_the_error_and_refuses_later_ones() {
    let sem = Arc::new(Semaphore::new(1));
    let held = sem.try_acquire().expect("free");
    let mut borrowed = Polled::new(sem.acquire());
    let mut many = Polled::new(sem.acquire_many(1));
    let mut owned = Polled::new(Arc::clone(&sem).acquire_owned());
    let mut many_owned = Polled::new(Arc::clone(&sem).acquire_many_owned(1));
    borrowed.pending();
    many.pending();
    owned.pending();
    many_owned.pending();
    assert!(!sem.is_closed());
    sem.close();
    assert!(sem.is_closed());
    sem.close();
    assert_eq!(
        (
            borrowed.wakes(),
            many.wakes(),
            owned.wakes(),
            many_owned.wakes()
        ),
        (1, 1, 1, 1),
        "every queued wait is woken"
    );
    assert!(borrowed.ready().is_err());
    assert!(many.ready().is_err());
    assert!(owned.ready().is_err());
    assert!(many_owned.ready().is_err());
    assert!(
        Polled::new(sem.acquire()).ready().is_err(),
        "later waits fail at once"
    );
    assert!(block_on(Arc::clone(&sem).acquire_many_owned(0)).is_err());
    assert_eq!(sem.available_permits(), 0);
    drop(held);
    assert_eq!(sem.available_permits(), 1, "no permit lost or made up");
    sem.add_permits(2);
    assert_eq!(sem.available_permits(), 3, "added permits are counted");
    assert!(sem.try_acquire().is_err(), "but never handed out");
}

#[test]
fn a_wait_that_was_handed_its_permit_before_the_close_keeps_it() {
    let sem = Semaphore::new(1);
    let mut held = sem.try_acquire().expect("free");
    let mut owed = Polled::new(sem.acquire());
    owed.pending();
    // Pass the waiter over until the permit is owed to it.
    for _ in 0..MAX_BYPASS {
        drop(held);
        held = sem.try_acquire().expect("a free permit is taken past it");
        owed.pending();
    }
    drop(held);
    sem.close();
    let permit = owed
        .ready()
        .expect("the permit was handed over before the close");
    assert_eq!(permit.num_permits(), 1);
    drop(permit);
    assert_eq!(sem.available_permits(), 1);
}

#[test]
fn a_wait_that_is_dropped_never_returns_more_than_it_took() {
    let sem = Arc::new(Semaphore::new(1));
    let held = sem.try_acquire().expect("free");
    let mut waiting = Polled::new(Arc::clone(&sem).acquire_owned());
    waiting.pending();
    sem.close();
    drop(waiting);
    drop(held);
    assert_eq!(sem.available_permits(), 1);

    let sem = Semaphore::new(1);
    let held = sem.try_acquire().expect("free");
    let mut owed = Polled::new(sem.acquire());
    owed.pending();
    let mut next = Polled::new(sem.acquire());
    next.pending();
    drop(held);
    drop(owed);
    assert_eq!(next.wakes(), 1, "the dropped wait woke the next one");
    drop(next.ready());
    assert_eq!(sem.available_permits(), 1);
}

#[test]
fn permits_forget_merge_and_split() {
    let sem = Arc::new(Semaphore::new(6));
    let mut all = sem.try_acquire_many(2).expect("free");
    all.merge(sem.try_acquire_many(3).expect("free"));
    assert_eq!(all.num_permits(), 5);
    let part = all.split(2).expect("held");
    assert_eq!((all.num_permits(), part.num_permits()), (3, 2));
    assert!(all.split(4).is_none(), "more than it holds");
    assert!(std::ptr::eq(all.semaphore(), &*sem));
    drop(part);
    assert_eq!(sem.available_permits(), 3);
    all.forget();
    assert_eq!(
        sem.available_permits(),
        3,
        "forgotten permits never come back"
    );

    let mut owned = Arc::clone(&sem).try_acquire_many_owned(2).expect("free");
    owned.merge(Arc::clone(&sem).try_acquire_owned().expect("free"));
    assert_eq!(owned.num_permits(), 3);
    let split = owned.split(1).expect("held");
    assert!(owned.split(3).is_none());
    assert!(Arc::ptr_eq(split.semaphore(), &sem));
    assert_eq!(format!("{split:?}"), "OwnedSemaphorePermit { permits: 1 }");
    split.forget();
    drop(owned);
    assert_eq!(sem.available_permits(), 2);
}

#[test]
#[should_panic(expected = "different semaphore instances")]
fn merging_permits_of_two_semaphores_is_refused() {
    let (a, b) = (Semaphore::new(1), Semaphore::new(1));
    let mut one = a.try_acquire().expect("free");
    one.merge(b.try_acquire().expect("free"));
}

#[test]
#[should_panic(expected = "different semaphore instances")]
fn merging_owned_permits_of_two_semaphores_is_refused() {
    let (a, b) = (Arc::new(Semaphore::new(1)), Arc::new(Semaphore::new(1)));
    let mut one = a.try_acquire_owned().expect("free");
    one.merge(b.try_acquire_owned().expect("free"));
}

#[test]
fn the_blocking_semaphore_calls_wait_on_a_plain_thread() {
    let sem = Arc::new(Semaphore::new(1));
    let held = sem.blocking_acquire().expect("open");
    let worker = {
        let sem = Arc::clone(&sem);
        std::thread::spawn(move || {
            drop(sem.blocking_acquire_many(1).expect("open"));
            drop(Arc::clone(&sem).blocking_acquire_owned().expect("open"));
            drop(
                Arc::clone(&sem)
                    .blocking_acquire_many_owned(1)
                    .expect("open"),
            );
            sem.blocking_acquire().map(drop)
        })
    };
    drop(held);
    assert!(worker.join().expect("worker").is_ok());
    let sem = Arc::new(Semaphore::new(0));
    let waiter = {
        let sem = Arc::clone(&sem);
        std::thread::spawn(move || sem.blocking_acquire().map(drop))
    };
    // Whenever the waiter parks or does not, the close ends its wait.
    sem.close();
    assert!(waiter.join().expect("waiter").is_err());
}
