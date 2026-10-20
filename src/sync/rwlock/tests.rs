//! Unit tests of the reader-writer lock and its guards.
use super::*;
use crate::sync::MAX_BYPASS;
use crate::sync::test_support::{Polled, block_on};
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering as StdOrdering};

/// A lock built without spin queues its contended readers and writers at
/// once: its budget is zero from the start and stays zero through a
/// contended write, its wake and its retry, where a spinning lock's
/// starts above zero.
#[test]
fn a_rwlock_built_without_spin_never_spins() {
    assert!(RwLock::new(()).spin_rounds() > 0, "a plain lock spins");
    let lock = RwLock::new_without_spin(0u8);
    assert_eq!(lock.spin_rounds(), 0);
    let read = lock.try_read().expect("free");
    let mut writer = Polled::new(lock.write());
    writer.pending();
    drop(read);
    assert_eq!(writer.wakes(), 1);
    *writer.ready() += 1;
    assert_eq!(lock.spin_rounds(), 0, "the lock never spins");
}

const ITERATIONS: usize = if cfg!(miri) { 20 } else { 2_000 };

#[test]
fn readers_share_and_a_writer_excludes() {
    let lock = RwLock::new(0);
    let a = lock.try_read().expect("free");
    let b = lock.try_read().expect("readers share");
    assert_eq!(
        lock.try_write().unwrap_err().to_string(),
        "operation would block"
    );
    drop((a, b));
    let w = lock.try_write().expect("free");
    assert!(lock.try_read().is_err());
    drop(w);
}

#[test]
fn readers_barge_past_a_waiting_writer_until_it_is_owed() {
    let lock = RwLock::new(());
    let mut inside = lock.try_read().expect("free");
    let mut writer = Polled::new(lock.write());
    writer.pending();
    for round in 0..MAX_BYPASS {
        let next = lock.try_read().expect("a read barges past the writer");
        drop(inside);
        inside = next;
        assert_eq!(
            writer.wakes() as u32,
            round + 1,
            "every release nudges the writer"
        );
        writer.pending();
    }
    assert!(lock.try_read().is_err(), "the writer is owed the lock now");
    drop(inside);
    assert_eq!(writer.wakes() as u32, MAX_BYPASS + 1);
    drop(writer.ready());
    assert!(lock.try_read().is_ok());
}

#[test]
fn a_released_writer_lets_the_queued_readers_in_together() {
    let lock = RwLock::new(());
    let w0 = lock.try_write().expect("free");
    let mut r1 = Polled::new(lock.read());
    r1.pending();
    let mut r2 = Polled::new(lock.read());
    r2.pending();
    drop(w0);
    assert_eq!((r1.wakes(), r2.wakes()), (1, 1));
    let (a, b) = (r1.ready(), r2.ready());
    drop((a, b));
}

#[test]
fn an_owned_write_outlives_the_borrow_and_keeps_its_place() {
    let lock = Arc::new(RwLock::new(0));
    let mut inside = lock.try_read().expect("free");
    // Made from a borrow that ends at once: the future owns the lock.
    let mut writer = Polled::new(Arc::clone(&lock).write_owned());
    writer.pending();
    for _ in 0..MAX_BYPASS {
        let next = lock.try_read().expect("a read barges past the writer");
        drop(inside);
        inside = next;
        writer.pending();
    }
    assert!(
        lock.try_read().is_err(),
        "the owned writer counted its bypasses like a borrowed one"
    );
    drop(inside);
    let mut guard = writer.ready();
    *guard += 1;
    assert!(lock.try_read().is_err(), "the guard holds the write");
    drop(guard);
    assert_eq!(*lock.try_read().expect("released"), 1);
}

#[test]
fn a_dropped_owned_write_passes_a_handoff_on() {
    let lock = Arc::new(RwLock::new(()));
    let mut inside = lock.try_read().expect("free");
    let mut writer = Polled::new(Arc::clone(&lock).write_owned());
    writer.pending();
    for _ in 0..MAX_BYPASS {
        let next = lock.try_read().expect("barge");
        drop(inside);
        inside = next;
        writer.pending();
    }
    let mut reader = Polled::new(lock.read());
    reader.pending();
    drop(inside);
    drop(writer);
    assert_eq!(reader.wakes(), 1, "the dropped handoff reached the reader");
    drop(reader.ready());
    assert!(lock.try_write().is_ok());
}

#[test]
fn a_withdrawn_writer_leaves_nothing_behind() {
    let lock = RwLock::new(());
    let r0 = lock.try_read().expect("free");
    Polled::new(lock.write()).pending();
    drop(r0);
    assert!(lock.try_write().is_ok());
}

#[test]
fn a_writer_handed_the_lock_but_dropped_passes_it_on() {
    let lock = RwLock::new(());
    let mut inside = lock.try_read().expect("free");
    let mut writer = Polled::new(lock.write());
    writer.pending();
    for _ in 0..MAX_BYPASS {
        let next = lock.try_read().expect("barge");
        drop(inside);
        inside = next;
        writer.pending();
    }
    let mut reader = Polled::new(lock.read());
    reader.pending();
    drop(inside);
    drop(writer);
    assert_eq!(reader.wakes(), 1, "the dropped handoff reached the reader");
    drop(reader.ready());
    assert!(lock.try_write().is_ok());
}

#[test]
fn one_upgradable_read_shares_with_readers_and_excludes_writers() {
    let lock = RwLock::new(1);
    let up = lock.try_upgradable_read().expect("free");
    let read = lock.try_read().expect("plain readers share with it");
    assert!(lock.try_upgradable_read().is_none(), "one at a time");
    assert!(lock.try_write().is_err());
    let up = up.try_upgrade().expect_err("a reader is inside");
    let mut upgrade = Polled::new(up.upgrade());
    upgrade.pending();
    assert!(lock.try_read().is_err(), "new readers are held back");
    drop(read);
    assert_eq!(upgrade.wakes(), 1);
    let mut write = upgrade.ready();
    *write += 1;
    drop(write);
    assert_eq!(*lock.try_read().expect("free"), 2);
}

#[test]
fn a_dropped_upgrade_releases_the_upgradable_read() {
    let lock = RwLock::new(());
    let up = lock.try_upgradable_read().expect("free");
    let read = lock.try_read().expect("share");
    Polled::new(up.upgrade()).pending();
    assert!(lock.try_read().is_ok(), "readers are welcome again");
    drop(read);
    assert!(lock.try_write().is_ok(), "nothing is left held");
}

#[test]
fn a_downgrade_turns_the_write_into_one_read_atomically() {
    let lock = Arc::new(RwLock::new(1));
    let mut write = lock.try_write().expect("free");
    *write += 1;
    let mut rival = Polled::new(Arc::clone(&lock).write_owned());
    rival.pending();
    let read = write.downgrade();
    assert_eq!(*read, 2, "the value the write left");
    assert!(lock.try_write().is_err(), "the read excludes writers");
    assert!(lock.try_read().is_ok(), "and shares with readers");
    drop(read);
    assert_eq!(rival.wakes(), 1, "the last read wakes the rival writer");
    let mut rival = rival.ready();
    *rival += 10;
    drop(rival);
    assert_eq!(*lock.try_read().expect("free"), 12);
}

#[test]
fn a_downgrade_lets_the_queued_readers_in_beside_it() {
    let lock = RwLock::new(5);
    let write = lock.try_write().expect("free");
    let mut first = Polled::new(lock.read());
    first.pending();
    let mut second = Polled::new(lock.read());
    second.pending();
    let mine = write.downgrade();
    assert_eq!((first.wakes(), second.wakes()), (1, 1), "both are woken");
    let (a, b) = (first.ready(), second.ready());
    assert_eq!((*mine, *a, *b), (5, 5, 5));
}

#[test]
fn a_downgrade_behind_a_queued_writer_keeps_the_writer_waiting() {
    let lock = RwLock::new(());
    let write = lock.try_write().expect("free");
    let mut writer = Polled::new(lock.write());
    writer.pending();
    let mut reader = Polled::new(lock.read());
    reader.pending();
    let mine = write.downgrade();
    assert_eq!(writer.wakes(), 1, "the front writer is woken to try");
    assert_eq!(reader.wakes(), 0, "a reader waits behind a writer");
    writer.pending();
    drop(mine);
    let guard = writer.ready();
    drop(guard);
    drop(reader.ready());
}

#[test]
fn a_downgrade_map_and_a_declined_one() {
    let lock = RwLock::new((1, "x"));
    let write = lock.try_write().expect("free");
    let write = RwLockWriteGuard::try_downgrade_map(write, |pair| (pair.0 > 1).then_some(&pair.0))
        .expect_err("one is not above one");
    assert!(lock.try_read().is_err(), "a declined map keeps the write");
    let read = RwLockWriteGuard::downgrade_map(write, |pair| &pair.1);
    assert_eq!(*read, "x");
    assert!(lock.try_read().is_ok(), "the write became a read");
    drop(read);
    let write = lock.try_write().expect("free");
    let read = RwLockWriteGuard::try_downgrade_map(write, |pair| Some(&pair.0))
        .expect("a number is there");
    assert_eq!(*read, 1);
}

#[test]
fn mapped_guards_hold_the_lock_and_release_it_on_drop() {
    let lock = Arc::new(RwLock::new((1, 2)));
    let read = RwLockReadGuard::map(lock.try_read().expect("free"), |pair| &pair.1);
    assert_eq!(*read, 2);
    let read = RwLockReadGuard::try_map(read, |n| (*n > 5).then_some(n)).expect_err("two");
    assert!(lock.try_write().is_err());
    drop(read);

    let mut write = RwLockWriteGuard::map(lock.try_write().expect("free"), |pair| &mut pair.0);
    *write += 10;
    let write =
        RwLockMappedWriteGuard::try_map(write, |n| (*n > 100).then_some(n)).expect_err("eleven");
    assert!(lock.try_read().is_err(), "the mapped write is held");
    let mut write = RwLockMappedWriteGuard::map(write, |n| n);
    *write += 1;
    drop(write);
    assert_eq!(lock.try_read().expect("free").0, 12);

    let whole = RwLockWriteGuard::into_mapped(lock.try_write().expect("free"));
    assert_eq!(whole.1, 2);
    drop(whole);

    let owned = Arc::clone(&lock).try_write_owned().expect("free");
    let mut owned = OwnedRwLockWriteGuard::map(owned, |pair| &mut pair.1);
    *owned = 7;
    assert!(Arc::ptr_eq(
        OwnedRwLockMappedWriteGuard::rwlock(&owned),
        &lock
    ));
    let owned = OwnedRwLockMappedWriteGuard::try_map(owned, |n| (*n > 100).then_some(n))
        .expect_err("seven");
    drop(OwnedRwLockMappedWriteGuard::map(owned, |n| n));
    let shared =
        OwnedRwLockReadGuard::map(Arc::clone(&lock).try_read_owned().expect("free"), |pair| {
            &pair.1
        });
    assert_eq!(*shared, 7);
    assert!(Arc::ptr_eq(OwnedRwLockReadGuard::rwlock(&shared), &lock));
    let shared =
        OwnedRwLockReadGuard::try_map(shared, |n| (*n > 100).then_some(n)).expect_err("seven");
    assert!(lock.try_write().is_err());
    drop(shared);
    assert!(lock.try_write().is_ok());
}

#[test]
fn owned_write_guards_downgrade_and_map() {
    let lock = Arc::new(RwLock::new((1, 2)));
    let write = block_on(Arc::clone(&lock).write_owned());
    assert!(Arc::ptr_eq(OwnedRwLockWriteGuard::rwlock(&write), &lock));
    let write = OwnedRwLockWriteGuard::try_map(write, |pair| (pair.0 > 5).then_some(&mut pair.0))
        .expect_err("one is small");
    let write =
        OwnedRwLockWriteGuard::try_downgrade_map(write, |pair| (pair.0 > 5).then_some(&pair.0))
            .expect_err("one is small");
    let read = OwnedRwLockWriteGuard::downgrade_map(write, |pair| &pair.1);
    assert_eq!(*read, 2);
    assert!(lock.try_read().is_ok());
    assert!(lock.try_write().is_err());
    drop(read);

    let mapped =
        OwnedRwLockWriteGuard::into_mapped(Arc::clone(&lock).try_write_owned().expect("free"));
    drop(mapped);
    let read = OwnedRwLockWriteGuard::downgrade(Arc::clone(&lock).try_write_owned().expect("free"));
    assert_eq!(read.0, 1);
    drop(read);
    let read = OwnedRwLockWriteGuard::try_downgrade_map(
        Arc::clone(&lock).try_write_owned().expect("free again"),
        |pair| Some(&pair.0),
    )
    .expect("a number is there");
    assert_eq!(*read, 1);
}

#[test]
fn at_most_max_readers_readers_are_inside() {
    let lock = RwLock::with_max_readers((), 2);
    let a = lock.try_read().expect("first");
    let b = lock.try_read().expect("second");
    assert!(lock.try_read().is_err(), "the third finds them all inside");
    let mut third = Polled::new(lock.read());
    third.pending();
    drop(a);
    assert_eq!(third.wakes(), 1, "a leaving reader wakes the waiting one");
    let third = third.ready();
    assert!(lock.try_read().is_err());
    drop((b, third));
    assert!(lock.try_write().is_ok());
}

#[test]
fn one_reader_at_a_time_still_lets_a_writer_through() {
    static LOCK: RwLock<u8> = RwLock::const_with_max_readers(0, 1);
    let only = LOCK.try_read().expect("first");
    assert!(LOCK.try_read().is_err());
    assert!(LOCK.try_write().is_err());
    drop(only);
    *LOCK.try_write().expect("free") += 1;
    let down = LOCK.try_write().expect("free").downgrade();
    assert_eq!(*down, 1);
    assert!(LOCK.try_read().is_err(), "the downgraded read is the one");
}

#[test]
#[should_panic(expected = "0 readers")]
fn a_lock_for_no_readers_fails_loudly() {
    let _ = RwLock::with_max_readers((), 0);
}

#[test]
#[should_panic(expected = "more than u32::MAX >> 3 readers")]
fn a_lock_for_too_many_readers_fails_loudly() {
    let _ = RwLock::const_with_max_readers((), u32::MAX);
}

#[test]
fn blocking_locks_wait_for_the_holder_on_another_thread() {
    let lock = Arc::new(RwLock::new(0));
    let held = lock.blocking_write();
    let waiter = {
        let lock = Arc::clone(&lock);
        std::thread::spawn(move || {
            *lock.blocking_write() += 1;
            assert_eq!(*lock.blocking_read(), 1);
        })
    };
    drop(held);
    waiter.join().expect("waiter");
}

#[test]
fn conversions_and_formatting() {
    let lock: RwLock<u8> = 4.into();
    assert_eq!(format!("{lock:?}"), "RwLock { data: 4 }");
    let write = lock.try_write().expect("free");
    assert_eq!(format!("{lock:?}"), "RwLock { data: <locked> }");
    assert_eq!(format!("{write} {write:?}"), "4 4");
    drop(write);
    assert_eq!(RwLock::<u8>::default().into_inner(), 0);
    let mut lock = RwLock::new(1);
    *lock.get_mut() += 1;
    let read = Arc::new(lock).try_read_owned().expect("free");
    assert_eq!(format!("{read} {read:?}"), "2 2");
}

#[test]
fn threads_see_readers_xor_one_writer() {
    let lock = Arc::new(RwLock::new(()));
    let inside = Arc::new(AtomicIsize::new(0));
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let (lock, inside) = (Arc::clone(&lock), Arc::clone(&inside));
            std::thread::spawn(move || {
                for i in 0..ITERATIONS {
                    match (i + t) % 4 {
                        0 => {
                            let _w = block_on(lock.write());
                            assert_eq!(inside.swap(-1, StdOrdering::SeqCst), 0);
                            std::hint::spin_loop();
                            assert_eq!(inside.swap(0, StdOrdering::SeqCst), -1);
                        }
                        1 => {
                            let up = block_on(lock.upgradable_read());
                            assert!(inside.load(StdOrdering::SeqCst) >= 0);
                            let _w = block_on(up.upgrade());
                            assert_eq!(inside.swap(-1, StdOrdering::SeqCst), 0);
                            assert_eq!(inside.swap(0, StdOrdering::SeqCst), -1);
                        }
                        _ => {
                            let _r = block_on(lock.read());
                            assert!(inside.fetch_add(1, StdOrdering::SeqCst) >= 0);
                            std::hint::spin_loop();
                            inside.fetch_sub(1, StdOrdering::SeqCst);
                        }
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("worker");
    }
    assert!(lock.try_write().is_ok());
}
