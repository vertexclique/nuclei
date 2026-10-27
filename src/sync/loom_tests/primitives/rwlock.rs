//! The reader-writer lock: exclusion, the upgradable read and a cancelled writer.

use super::*;

contended!(
    an_upgradable_read_upgrades_while_readers_and_a_writer_contend,
    an_upgradable_read_upgrades_while_readers_and_a_writer_contend_at_bound_2,
    10,
    || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let upgrader = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let upgradable = block_on(lock.upgradable_read());
                let _ = read(&upgradable);
                bump(&block_on(upgradable.upgrade()));
            })
        };
        let reader = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let _ = read(&block_on(lock.read()));
            })
        };
        bump(&block_on(lock.write()));
        upgrader.join().expect("upgrader");
        reader.join().expect("reader");
        assert_eq!(read(&lock.try_write().expect("free")), 2);
    },
    // The bound-2 twin runs a smaller scenario. The one above did not finish
    // at bound 2 (a run was stopped after 33 hours): three threads each park
    // and wake on the lock, and two preemptions among them is too many
    // interleavings. Here the upgradable read is held from the start, so no
    // thread waits for it, and the reader only tries (`try_read`), so it
    // never parks. What still races at bound 2 is the upgrade against a
    // reader coming or going and against a writer queued behind it, with
    // every wake the upgrade and the writer owe each other. Measured: 171149
    // interleavings in 10 s. With a blocking reader in place of the trying
    // one it did not finish in 30 minutes.
    || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let upgradable = lock.try_upgradable_read().expect("free");
        let reader = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let _ = lock.try_read().map(|shared| read(&shared));
            })
        };
        let writer = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || bump(&block_on(lock.write())))
        };
        bump(&block_on(upgradable.upgrade()));
        reader.join().expect("reader");
        writer.join().expect("writer");
        assert_eq!(read(&lock.try_write().expect("free")), 2);
    }
);

#[test]
fn a_dropped_upgrade_racing_the_last_reader_leaves_nothing_held() {
    explore(
        "a_dropped_upgrade_racing_the_last_reader_leaves_nothing_held",
        3,
        5,
        || {
            let lock = Arc::new(RwLock::new(()));
            let upgradable = lock.try_upgradable_read().expect("free");
            let reader = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || drop(lock.try_read()))
            };
            poll_once_then_drop(upgradable.upgrade());
            reader.join().expect("reader");
            assert!(lock.try_write().is_ok(), "nothing was left held");
        },
    );
}

#[test]
fn a_rwlock_writer_and_readers_exclude_each_other() {
    explore(
        "a_rwlock_writer_and_readers_exclude_each_other",
        // SHRUNK: one preemption where there were two (2.5 million
        // interleavings, 4 minutes; now about 96 thousand), as the other
        // models whose three threads all wait for one lock run. The scenario
        // is unchanged: a writer and two readers, any of them first.
        1,
        10,
        || {
            let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
            let writer = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || bump(&block_on(lock.write())))
            };
            let reader = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    let _ = read(&block_on(lock.read()));
                })
            };
            let _ = read(&block_on(lock.read()));
            writer.join().expect("writer");
            reader.join().expect("reader");
            assert_eq!(read(&lock.try_write().expect("free")), 1);
        },
    );
}

contended!(
    a_cancelled_rwlock_writer_racing_the_last_reader_loses_nothing,
    a_cancelled_rwlock_writer_racing_the_last_reader_loses_nothing_at_bound_2,
    10,
    || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let held = lock.try_read().expect("free");
        let canceller = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || poll_once_then_drop(lock.write()))
        };
        let writer = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || bump(&block_on(lock.write())))
        };
        drop(held);
        canceller.join().expect("canceller");
        writer.join().expect("writer");
        assert_eq!(read(&lock.try_write().expect("the lock was not leaked")), 1);
    },
    // The bound-2 twin: the cancelled writer is queued before the other
    // writer exists, so the order of the two arrivals is fixed and only the
    // cancellation races the second writer's arrival and the last reader's
    // release.
    || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let held = lock.try_read().expect("free");
        let queued = Arc::new(AtomicBool::new(false));
        let canceller = {
            let (lock, flag) = (Arc::clone(&lock), Arc::clone(&queued));
            thread::spawn(move || poll_once_then_drop(flag_when_queued(lock.write(), &flag)))
        };
        wait_for(&queued);
        let writer = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || bump(&block_on(lock.write())))
        };
        drop(held);
        canceller.join().expect("canceller");
        writer.join().expect("writer");
        assert_eq!(read(&lock.try_write().expect("the lock was not leaked")), 1);
    }
);

/// An upgrade waits for a reader and is dropped. While its drop is
/// under way the reader leaves, which runs a drain pass, and a second task
/// takes the upgradable read; then the first task tries to write. The
/// upgradable read is its holder's own until that holder lets it go:
/// neither the dropped upgrade nor the pass may spend the bits the second
/// task raised, so no writer is ever inside while the second task holds it.
#[test]
fn a_dropped_upgrade_never_lets_a_writer_in_beside_the_next_upgradable_read() {
    explore(
        "a_dropped_upgrade_never_lets_a_writer_in_beside_the_next_upgradable_read",
        3,
        100,
        || {
            let lock = Arc::new(RwLock::new(()));
            let upgradable = lock.try_upgradable_read().expect("free");
            let inside = Arc::new(AtomicBool::new(false));
            let held = Arc::new(AtomicBool::new(false));
            let second = {
                let (lock, inside, held) =
                    (Arc::clone(&lock), Arc::clone(&inside), Arc::clone(&held));
                thread::spawn(move || {
                    let shared = lock.try_read().expect("readers share with it");
                    inside.store(true, Ordering::Release);
                    // Lets the upgrade begin, and its drop, while the reader
                    // is inside, without spending a preemption.
                    thread::yield_now();
                    // The reader leaves: the upgrade may be waiting for it.
                    drop(shared);
                    if let Some(next) = lock.try_upgradable_read() {
                        held.store(true, Ordering::Release);
                        // The same, while this task holds it.
                        thread::yield_now();
                        held.store(false, Ordering::Release);
                        drop(next);
                    }
                })
            };
            wait_for(&inside);
            poll_once_then_drop(upgradable.upgrade());
            if let Ok(alone) = lock.try_write() {
                assert!(
                    !held.load(Ordering::Acquire),
                    "a writer got in while the upgradable read was held"
                );
                drop(alone);
            }
            second.join().expect("second");
            assert!(lock.try_write().is_ok(), "nothing was left held");
        },
    );
}

/// A step the drain no longer takes, rebuilt out of plain loom atomics: a
/// drain pass that reads the upgrading bit to decide whether the upgrade in
/// its slot was dropped, and takes the write for it when no reader is inside.
/// The bit it reads may be the next holder's, and the write it takes and puts
/// back spends the upgradable read whoever holds it, so a writer gets in
/// beside that holder.
#[test]
#[should_panic(expected = "a writer got in while the upgradable read was held")]
fn calibration_a_pass_that_takes_the_write_for_a_dropped_upgrade_lets_a_writer_in() {
    const WRITER: usize = 1;
    const UPGRADABLE: usize = 1 << 1;
    const UPGRADING: usize = 1 << 2;
    const ONE_READER: usize = 1 << 3;
    explore(
        "calibration_a_pass_that_takes_the_write_for_a_dropped_upgrade_lets_a_writer_in",
        3,
        1,
        || {
            // The first task holds the upgradable read, its upgrade waits
            // for one reader, and its node is in the pass's slot.
            let state = Arc::new(AtomicUsize::new(UPGRADABLE | UPGRADING | ONE_READER));
            let withdrawn = Arc::new(AtomicBool::new(false));
            let held = Arc::new(AtomicBool::new(false));
            let second = {
                let (state, withdrawn, held) = (
                    Arc::clone(&state),
                    Arc::clone(&withdrawn),
                    Arc::clone(&held),
                );
                thread::spawn(move || {
                    // The reader leaves and runs the pass, the old way.
                    state.fetch_sub(ONE_READER, Ordering::AcqRel);
                    let seen = state.load(Ordering::Acquire);
                    if seen & UPGRADING != 0
                        && seen >> 3 == 0
                        && state
                            .compare_exchange(
                                seen,
                                (seen & !(UPGRADABLE | UPGRADING)) | WRITER,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                        && withdrawn.load(Ordering::Acquire)
                    {
                        // The node withdrew: the write taken for it goes back.
                        state.fetch_and(!WRITER, Ordering::AcqRel);
                    }
                    // Then it takes the upgradable read if it is free.
                    let free = state.load(Ordering::Acquire);
                    if free & (WRITER | UPGRADABLE) == 0
                        && state
                            .compare_exchange(
                                free,
                                free | UPGRADABLE,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                    {
                        held.store(true, Ordering::Release);
                        thread::yield_now();
                        held.store(false, Ordering::Release);
                        state.fetch_and(!UPGRADABLE, Ordering::AcqRel);
                    }
                })
            };
            // The first task drops its upgrade: its node withdraws, then it
            // lets the upgradable read go.
            withdrawn.store(true, Ordering::Release);
            state.fetch_and(!(UPGRADABLE | UPGRADING), Ordering::AcqRel);
            // Then it tries to write.
            if state
                .compare_exchange(0, WRITER, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                assert!(
                    !held.load(Ordering::Acquire),
                    "a writer got in while the upgradable read was held"
                );
                state.fetch_and(!WRITER, Ordering::AcqRel);
            }
            second.join().expect("second");
        },
    );
}
