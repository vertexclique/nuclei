//! The loom models of the state transitions added with the completed API:
//! a semaphore that closes, a write that becomes a read (and the reader
//! bound), the notification calls, and the cells. They use the helpers and
//! the `explore` driver of `tests/sync_loom.rs`.

use super::*;

// === a semaphore that closes ===

/// A close races a queued acquire. The only permit is held throughout, so
/// the wait can only end in the error, and it must end: a wait the close did
/// not wake is parked for good, which loom reports as a deadlock.
#[test]
fn a_close_ends_a_queued_acquire() {
    explore("a_close_ends_a_queued_acquire", 3, 5, || {
        let sem = Arc::new(Semaphore::new(1));
        let held = sem.try_acquire().expect("free");
        let waiter = {
            let sem = Arc::clone(&sem);
            thread::spawn(move || block_on(sem.acquire()).is_err())
        };
        sem.close();
        assert!(
            waiter.join().expect("waiter"),
            "the permit was held, so the wait could only fail"
        );
        assert_eq!(sem.try_acquire().unwrap_err(), TryAcquireError::Closed);
        drop(held);
        assert_eq!(sem.available_permits(), 1);
    });
}

/// The permit is released while a close and an acquire race for it. The
/// acquire either got the permit before the close (and returns it) or failed;
/// the semaphore ends closed with the one permit free either way.
#[test]
fn a_close_racing_a_release_and_an_acquire_keeps_the_permit() {
    explore(
        "a_close_racing_a_release_and_an_acquire_keeps_the_permit",
        2,
        5,
        || {
            let sem = Arc::new(Semaphore::new(1));
            let held = sem.try_acquire().expect("free");
            let waiter = {
                let sem = Arc::clone(&sem);
                thread::spawn(move || block_on(sem.acquire()).map(drop).is_ok())
            };
            let closer = {
                let sem = Arc::clone(&sem);
                thread::spawn(move || sem.close())
            };
            drop(held);
            waiter.join().expect("waiter");
            closer.join().expect("closer");
            assert!(sem.is_closed());
            assert_eq!(sem.available_permits(), 1, "a permit leaked or was made up");
            assert!(
                sem.try_acquire().is_err(),
                "a closed semaphore hands nothing out"
            );
        },
    );
}

/// An acquire polled once and dropped races a close and the release of the
/// permit it queued behind: whatever the drop was handed goes back, and
/// nothing is returned twice.
#[test]
fn a_cancelled_acquire_racing_a_close_returns_each_permit_once() {
    explore(
        "a_cancelled_acquire_racing_a_close_returns_each_permit_once",
        2,
        5,
        || {
            let sem = Arc::new(Semaphore::new(1));
            let held = sem.try_acquire().expect("free");
            let canceller = {
                let sem = Arc::clone(&sem);
                thread::spawn(move || poll_once_then_drop(sem.acquire()))
            };
            let closer = {
                let sem = Arc::clone(&sem);
                thread::spawn(move || sem.close())
            };
            drop(held);
            canceller.join().expect("canceller");
            closer.join().expect("closer");
            assert_eq!(sem.available_permits(), 1, "a permit leaked or was doubled");
        },
    );
}

/// Two waiters of different weights queue behind a holder of every permit,
/// which lets go while a close races it. Never more permits out than there
/// are, and every permit is back at the end.
#[test]
fn a_close_never_double_counts_a_permit() {
    explore("a_close_never_double_counts_a_permit", 1, 5, || {
        let sem = Arc::new(Semaphore::new(2));
        let out = Arc::new(AtomicUsize::new(0));
        let held = sem.try_acquire_many(2).expect("free");
        let takers: Vec<_> = [2u32, 1]
            .into_iter()
            .map(|n| {
                let (sem, out) = (Arc::clone(&sem), Arc::clone(&out));
                thread::spawn(move || {
                    if let Ok(permit) = block_on(sem.acquire_many(n)) {
                        let now = out.fetch_add(n as usize, Ordering::SeqCst) + n as usize;
                        assert!(now <= 2, "overdrawn");
                        out.fetch_sub(n as usize, Ordering::SeqCst);
                        drop(permit);
                    }
                })
            })
            .collect();
        drop(held);
        sem.close();
        for taker in takers {
            taker.join().expect("taker");
        }
        assert_eq!(sem.available_permits(), 2, "a permit leaked or was doubled");
    });
}

/// A close does not always beat a release that came before it in the
/// closer's own order: the wait may be served first. Expecting it to always
/// fail must itself fail.
#[test]
#[should_panic(expected = "the release came first, yet the wait failed")]
fn calibration_a_release_before_a_close_can_still_serve_the_wait() {
    explore(
        "calibration_a_release_before_a_close_can_still_serve_the_wait",
        3,
        1,
        || {
            let sem = Arc::new(Semaphore::new(1));
            let held = sem.try_acquire().expect("free");
            let waiter = {
                let sem = Arc::clone(&sem);
                thread::spawn(move || block_on(sem.acquire()).is_ok())
            };
            drop(held);
            sem.close();
            assert!(
                waiter.join().expect("waiter"),
                "the release came first, yet the wait failed"
            );
        },
    );
}

// === a write that becomes a read ===

/// The holder of the write downgrades while a writer waits for the lock. No
/// writer is ever inside beside the read: the value the first write left is
/// the value the read sees, and loom fails a writer that enters while the
/// read is being looked at.
#[test]
fn a_downgrade_lets_no_writer_in_between() {
    explore("a_downgrade_lets_no_writer_in_between", 2, 10, || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let write = block_on(lock.write());
        bump(&write);
        let writer = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || bump(&block_on(lock.write())))
        };
        let shared = RwLockWriteGuard::downgrade(write);
        assert_eq!(read(&shared), 1, "a writer got in before the read");
        drop(shared);
        writer.join().expect("writer");
        assert_eq!(read(&lock.try_read().expect("free")), 2);
    });
}

/// The reader queued behind the write is woken by the downgrade, not by
/// the release that comes after it: the holder keeps its read until the
/// reader reports it is inside, so a downgrade that woke nobody leaves the
/// reader parked while the holder waits for it.
#[test]
fn a_downgrade_wakes_a_queued_reader_to_share() {
    explore("a_downgrade_wakes_a_queued_reader_to_share", 3, 5, || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let write = block_on(lock.write());
        bump(&write);
        let inside = Arc::new(AtomicBool::new(false));
        let reader = {
            let (lock, inside) = (Arc::clone(&lock), Arc::clone(&inside));
            thread::spawn(move || {
                let shared = block_on(lock.read());
                let seen = read(&shared);
                inside.store(true, Ordering::Release);
                seen
            })
        };
        let mine = write.downgrade();
        wait_for(&inside);
        assert_eq!(read(&mine), 1);
        drop(mine);
        assert_eq!(reader.join().expect("reader"), 1);
    });
}

/// A downgrade written as a release followed by a read lets a writer in
/// between, and the read sees what the writer left.
#[test]
#[should_panic(expected = "a writer got in between the release and the read")]
fn calibration_a_downgrade_that_releases_first_lets_a_writer_in() {
    explore(
        "calibration_a_downgrade_that_releases_first_lets_a_writer_in",
        2,
        1,
        || {
            let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
            let write = block_on(lock.write());
            bump(&write);
            let writer = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || bump(&block_on(lock.write())))
            };
            drop(write);
            let shared = block_on(lock.read());
            assert_eq!(
                read(&shared),
                1,
                "a writer got in between the release and the read"
            );
            drop(shared);
            writer.join().expect("writer");
        },
    );
}

/// A lock built for one reader never has two inside, and a reader that
/// finds the bound reached is woken by the one that leaves.
///
/// SHRUNK to two readers: with three, the cap, the wakes and the handoff made
/// the search run longer than ten minutes.
#[test]
fn a_lock_with_one_reader_never_has_two_inside() {
    explore("a_lock_with_one_reader_never_has_two_inside", 3, 5, || {
        let lock = Arc::new(RwLock::with_max_readers((), 1));
        let inside = Arc::new(AtomicUsize::new(0));
        let enter = |lock: &RwLock<()>, inside: &AtomicUsize| {
            let _guard = block_on(lock.read());
            assert_eq!(
                inside.fetch_add(1, Ordering::SeqCst),
                0,
                "two readers inside"
            );
            thread::yield_now();
            inside.fetch_sub(1, Ordering::SeqCst);
        };
        let reader = {
            let (lock, inside) = (Arc::clone(&lock), Arc::clone(&inside));
            thread::spawn(move || enter(&lock, &inside))
        };
        enter(&lock, &inside);
        reader.join().expect("reader");
        assert!(lock.try_write().is_ok(), "nothing was left held");
    });
}

/// A blocking lock is woken by the unlock like an awaited one.
#[test]
fn a_blocking_lock_is_woken_by_the_unlock() {
    explore("a_blocking_lock_is_woken_by_the_unlock", 3, 5, || {
        let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
        let held = mutex.blocking_lock();
        let waiter = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || bump(&mutex.blocking_lock()))
        };
        bump(&held);
        drop(held);
        waiter.join().expect("waiter");
        assert_eq!(read(&mutex.try_lock().expect("free")), 2);
    });
}

// === Notify ===

/// A `Notified` made before `notify_waiters` ends however its first poll
/// falls against the call: queued and woken by it, or ready at once.
#[test]
fn a_notified_made_before_notify_waiters_ends_wherever_it_is_first_polled() {
    explore(
        "a_notified_made_before_notify_waiters_ends_wherever_it_is_first_polled",
        3,
        5,
        || {
            let notify = Arc::new(Notify::new());
            let waiting = notify.notified();
            let notifier = {
                let notify = Arc::clone(&notify);
                thread::spawn(move || notify.notify_waiters())
            };
            block_on(waiting);
            notifier.join().expect("notifier");
            // The broadcast stored no permit.
            assert!(
                poll_once_then_drop(notify.notified()),
                "a broadcast stored a permit"
            );
        },
    );
}

/// Two `notify_one` with nobody waiting store one permit between them: the
/// first `notified()` is ready, the second is not.
#[test]
fn notify_one_with_nobody_waiting_stores_exactly_one_permit() {
    explore(
        "notify_one_with_nobody_waiting_stores_exactly_one_permit",
        3,
        5,
        || {
            let notify = Arc::new(Notify::new());
            let senders: Vec<_> = (0..2)
                .map(|_| {
                    let notify = Arc::clone(&notify);
                    thread::spawn(move || notify.notify_one())
                })
                .collect();
            for sender in senders {
                sender.join().expect("sender");
            }
            assert!(
                !poll_once_then_drop(notify.notified()),
                "the stored permit was lost"
            );
            assert!(
                poll_once_then_drop(notify.notified()),
                "two notifications stored two permits"
            );
        },
    );
}

/// The same for a `notify_one` and a `notify_last` racing.
#[test]
fn notify_one_and_notify_last_with_nobody_waiting_store_one_permit() {
    explore(
        "notify_one_and_notify_last_with_nobody_waiting_store_one_permit",
        3,
        5,
        || {
            let notify = Arc::new(Notify::new());
            let one = {
                let notify = Arc::clone(&notify);
                thread::spawn(move || notify.notify_one())
            };
            let last = {
                let notify = Arc::clone(&notify);
                thread::spawn(move || notify.notify_last())
            };
            one.join().expect("one");
            last.join().expect("last");
            assert!(
                !poll_once_then_drop(notify.notified()),
                "the permit was lost"
            );
            assert!(
                poll_once_then_drop(notify.notified()),
                "two permits were stored"
            );
        },
    );
}

#[test]
fn notify_last_is_never_lost() {
    explore("notify_last_is_never_lost", 3, 5, || {
        let notify = Arc::new(Notify::new());
        let waiter = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || block_on(notify.notified()))
        };
        notify.notify_last();
        waiter.join().expect("waiter");
    });
}

/// A queued `Notified` that is dropped passes a `notify_last` it was handed
/// to the next waiter, whether it took it, dropped it or never saw it.
#[test]
fn a_dropped_notified_passes_its_notify_last_on() {
    explore("a_dropped_notified_passes_its_notify_last_on", 1, 5, || {
        let notify = Arc::new(Notify::new());
        let canceller = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || !poll_once_then_drop(notify.notified()))
        };
        let waiter = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || block_on(notify.notified()))
        };
        notify.notify_last();
        if canceller.join().expect("canceller") {
            notify.notify_last();
        }
        waiter.join().expect("waiter");
    });
}

/// A future that was enabled is queued before its first poll: a `notify_one`
/// sent from another thread reaches it, and no permit is left over.
#[test]
fn an_enabled_notified_receives_a_notify_one_sent_before_its_first_poll() {
    explore(
        "an_enabled_notified_receives_a_notify_one_sent_before_its_first_poll",
        3,
        5,
        || {
            let notify = Arc::new(Notify::new());
            let mut waiting = Box::pin(notify.notified());
            assert!(!waiting.as_mut().enable(), "nothing was sent yet");
            let sender = {
                let notify = Arc::clone(&notify);
                thread::spawn(move || notify.notify_one())
            };
            block_on(waiting.as_mut());
            sender.join().expect("sender");
            assert!(
                poll_once_then_drop(notify.notified()),
                "the notification was stored as well as delivered"
            );
        },
    );
}

/// An enabled future asks `enable` again and again while a `notify_one` from
/// another thread is claiming its waker: the asking only looks, so it never
/// arms a waker a grant is taking, and it reports ready once the grant is
/// in.
#[test]
fn an_enabled_notified_sees_a_concurrent_notify_one_by_asking_again() {
    explore(
        "an_enabled_notified_sees_a_concurrent_notify_one_by_asking_again",
        3,
        5,
        || {
            let notify = Arc::new(Notify::new());
            let mut waiting = Box::pin(notify.notified());
            assert!(!waiting.as_mut().enable(), "nothing was sent yet");
            let sender = {
                let notify = Arc::clone(&notify);
                thread::spawn(move || notify.notify_one())
            };
            while !waiting.as_mut().enable() {
                thread::yield_now();
            }
            sender.join().expect("sender");
            assert!(
                poll_once_then_drop(notify.notified()),
                "the notification was stored as well as delivered"
            );
        },
    );
}

// === the cells ===

/// A `set` and an initializer race; the cell ends with the value of the one
/// that won, and the other never publishes.
#[test]
fn a_set_racing_an_initializer_leaves_one_value() {
    explore("a_set_racing_an_initializer_leaves_one_value", 3, 5, || {
        let cell = Arc::new(OnceCell::new());
        let setter = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || cell.set(1).is_ok())
        };
        let got = *block_on(cell.get_or_init(|| async { 2 }));
        let set_won = setter.join().expect("setter");
        assert_eq!(Some(&got), cell.get());
        assert_eq!(got, if set_won { 1 } else { 2 });
    });
}

/// A `set`, which writes the cell's own slot, races a racing builder, which
/// publishes a box: exactly one of them wins, and both see the winner.
#[test]
fn a_set_racing_a_racing_builder_leaves_one_value() {
    explore(
        "a_set_racing_a_racing_builder_leaves_one_value",
        3,
        5,
        || {
            let cell = Arc::new(OnceCell::new());
            let setter = {
                let cell = Arc::clone(&cell);
                thread::spawn(move || cell.set(1).is_ok())
            };
            let built = *cell.get_or_init_racy(|| 2);
            let set_won = setter.join().expect("setter");
            assert_eq!(Some(&built), cell.get());
            assert_eq!(built, if set_won { 1 } else { 2 });
        },
    );
}

/// Two `set` calls race and a waiter waits: exactly one `set` wins, and the
/// waiter is woken with the winner's value.
#[test]
fn one_set_wins_and_wakes_the_waiter() {
    explore("one_set_wins_and_wakes_the_waiter", 2, 5, || {
        let once = Arc::new(SetOnce::new());
        let setters: Vec<_> = [1usize, 2]
            .into_iter()
            .map(|n| {
                let once = Arc::clone(&once);
                thread::spawn(move || once.set(n).is_ok())
            })
            .collect();
        let waited = *block_on(once.wait());
        let wins = setters
            .into_iter()
            .map(|setter| setter.join().expect("setter"))
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1, "exactly one set wins");
        assert_eq!(Some(&waited), once.get());
    });
}

/// Three parties meet, whichever thread arrives last: one leader, and every
/// party is released.
///
/// SHRUNK to one generation: two generations of three parties did not finish
/// in ten minutes. The generation hand-over is covered by the two-party
/// models above.
#[test]
fn a_barrier_of_three_releases_the_group_with_one_leader() {
    explore(
        "a_barrier_of_three_releases_the_group_with_one_leader",
        1,
        5,
        || {
            let barrier = Arc::new(Barrier::new(3));
            let parties: Vec<_> = (0..2)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || block_on(barrier.wait()).is_leader())
                })
                .collect();
            let mine = block_on(barrier.wait()).is_leader();
            let leaders = parties
                .into_iter()
                .map(|party| party.join().expect("party"))
                .chain([mine])
                .filter(|leader| *leader)
                .count();
            assert_eq!(leaders, 1, "exactly one leader");
        },
    );
}
