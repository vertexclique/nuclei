//! The reader-writer lock: construction, the `try_*` and owned forms, the guards' `map` family, downgrade and
//! the reader bound.

use super::*;

// === RwLock ===

#[test]
fn a_rwlock_is_built_and_unwrapped() {
    static LOCK: RwLock<u32> = RwLock::const_new(1);
    *block_on(LOCK.write()) += 1;
    assert_eq!(*LOCK.try_read().expect("free"), 2);

    let mut lock = RwLock::new(String::from("a"));
    lock.get_mut().push('b');
    assert_eq!(lock.into_inner(), "ab");
    let from: RwLock<u8> = 5.into();
    assert_eq!(from.into_inner(), 5);
    assert_eq!(RwLock::<u8>::default().into_inner(), 0);
    let lock = RwLock::new(7);
    assert_eq!(format!("{lock:?}"), "RwLock { data: 7 }");
    let write = lock.try_write().expect("free");
    assert_eq!(format!("{lock:?}"), "RwLock { data: <locked> }");
    assert_eq!(format!("{write} {write:?}"), "7 7");
}

#[test]
fn the_try_forms_report_would_block() {
    let lock = Arc::new(RwLock::new(0));
    let read = lock.try_read().expect("free");
    let err = lock.try_write().unwrap_err();
    assert_eq!(err.to_string(), "operation would block");
    assert!(Arc::clone(&lock).try_write_owned().is_err());
    assert!(lock.try_read().is_ok(), "readers share");
    assert!(Arc::clone(&lock).try_read_owned().is_ok());
    drop(read);
    let write = Arc::clone(&lock).try_write_owned().expect("free");
    assert!(lock.try_read().is_err());
    assert!(Arc::clone(&lock).try_read_owned().is_err());
    assert!(lock.try_write().is_err());
    assert!(Arc::ptr_eq(OwnedRwLockWriteGuard::rwlock(&write), &lock));
}

#[test]
fn the_owned_calls_take_the_arc_by_value() {
    let lock = Arc::new(RwLock::new(1));
    let read = block_on(Arc::clone(&lock).read_owned());
    let mut writer = Polled::new(Arc::clone(&lock).write_owned());
    writer.pending();
    assert!(Arc::ptr_eq(OwnedRwLockReadGuard::rwlock(&read), &lock));
    drop(read);
    assert_eq!(writer.wakes(), 1);
    let mut write = writer.ready();
    *write += 1;
    assert_eq!(format!("{write} {write:?}"), "2 2");
    drop(write);
    let read = Arc::clone(&lock).try_read_owned().expect("free");
    assert_eq!(format!("{read} {read:?}"), "2 2");
}

#[test]
fn the_blocking_rwlock_calls_wait_on_a_plain_thread() {
    let lock = Arc::new(RwLock::new(0));
    let held = lock.blocking_write();
    let worker = {
        let lock = Arc::clone(&lock);
        std::thread::spawn(move || {
            *lock.blocking_write() += 1;
            assert_eq!(*lock.blocking_read(), 1);
        })
    };
    drop(held);
    worker.join().expect("worker");
    let held = lock.blocking_read();
    let reader = {
        let lock = Arc::clone(&lock);
        std::thread::spawn(move || *lock.blocking_read())
    };
    assert_eq!(reader.join().expect("reader"), 1, "readers share");
    drop(held);
}

#[test]
fn a_downgrade_is_atomic_and_keeps_the_value_the_write_left() {
    let lock = Arc::new(RwLock::new(1));
    let mut write = block_on(lock.write());
    *write = 5;
    let mut rival = Polled::new(Arc::clone(&lock).write_owned());
    rival.pending();
    let read = RwLockWriteGuard::downgrade(write);
    assert_eq!(*read, 5, "the rival writer did not get in between");
    assert!(lock.try_write().is_err());
    assert!(lock.try_read().is_ok(), "the write became a shared read");
    drop(read);
    let mut rival = rival.ready();
    *rival += 1;
    drop(rival);
    assert_eq!(*lock.try_read().expect("free"), 6);

    let write = Arc::clone(&lock).try_write_owned().expect("free");
    let read = write.downgrade();
    assert_eq!(*read, 6);
    assert!(lock.try_write().is_err());
    drop(read);
    assert!(lock.try_write().is_ok());
}

#[test]
fn a_downgrade_wakes_the_readers_queued_behind_the_write() {
    let lock = RwLock::new(3);
    let write = lock.try_write().expect("free");
    let mut readers: Vec<_> = (0..3).map(|_| Polled::new(lock.read())).collect();
    for reader in &mut readers {
        reader.pending();
    }
    let mine = write.downgrade();
    for reader in &mut readers {
        assert_eq!(reader.wakes(), 1, "a queued reader is woken to share");
        assert_eq!(*reader.ready(), 3);
    }
    assert_eq!(*mine, 3);
}

#[test]
fn a_downgrade_does_not_let_a_queued_writer_ahead_of_the_read() {
    let lock = RwLock::new(());
    let write = lock.try_write().expect("free");
    let mut writer = Polled::new(lock.write());
    writer.pending();
    let mut reader = Polled::new(lock.read());
    reader.pending();
    let mine = write.downgrade();
    assert_eq!(reader.wakes(), 0, "a reader waits behind a queued writer");
    writer.pending();
    drop(mine);
    drop(writer.ready());
    drop(reader.ready());
}

#[test]
fn downgrade_map_and_its_declined_form() {
    let lock = RwLock::new((1, "x"));
    let write = lock.try_write().expect("free");
    let write = RwLockWriteGuard::try_downgrade_map(write, |pair| (pair.0 > 1).then_some(&pair.0))
        .expect_err("one is not above one");
    assert!(
        lock.try_read().is_err(),
        "a declined map neither releases nor downgrades"
    );
    let read = RwLockWriteGuard::downgrade_map(write, |pair| &pair.1);
    assert_eq!(*read, "x");
    assert!(lock.try_read().is_ok(), "the write became a read");
    drop(read);

    let arc = Arc::new(RwLock::new((1, 2)));
    let write = block_on(Arc::clone(&arc).write_owned());
    let write =
        OwnedRwLockWriteGuard::try_downgrade_map(write, |pair| (pair.0 > 1).then_some(&pair.0))
            .expect_err("one is not above one");
    assert!(arc.try_read().is_err());
    let read = OwnedRwLockWriteGuard::downgrade_map(write, |pair| &pair.1);
    assert_eq!(*read, 2);
    assert!(arc.try_read().is_ok());
    drop(read);
    let write = block_on(Arc::clone(&arc).write_owned());
    let read =
        OwnedRwLockWriteGuard::try_downgrade_map(write, |pair| Some(&pair.0)).expect("found");
    assert_eq!(*read, 1);
}

#[test]
fn read_guards_map_to_components() {
    let lock = Arc::new(RwLock::new((1, String::from("x"))));
    let read = RwLockReadGuard::map(block_on(lock.read()), |pair| &pair.1);
    assert_eq!(format!("{read} {read:?}"), "x \"x\"");
    let read = RwLockReadGuard::try_map(read, |text| text.get(3..)).expect_err("short");
    assert!(lock.try_write().is_err(), "a declined map keeps the read");
    let read = RwLockReadGuard::try_map(read, |text| text.get(..1)).expect("found");
    assert_eq!(&*read, "x");
    drop(read);
    assert!(lock.try_write().is_ok(), "the mapped read released on drop");

    let owned = block_on(Arc::clone(&lock).read_owned());
    let owned = OwnedRwLockReadGuard::map(owned, |pair| &pair.0);
    assert_eq!(*owned, 1);
    let owned = OwnedRwLockReadGuard::try_map(owned, |n| (*n > 5).then_some(n)).expect_err("small");
    let owned = OwnedRwLockReadGuard::try_map(owned, |n| Some(n)).expect("found");
    assert!(Arc::ptr_eq(OwnedRwLockReadGuard::rwlock(&owned), &lock));
    assert_eq!(format!("{owned} {owned:?}"), "1 1");
    drop(lock);
    assert_eq!(*owned, 1, "the guard keeps the lock alive");
}

#[test]
fn write_guards_map_to_components_and_into_mapped() {
    let lock = Arc::new(RwLock::new((1, 2)));
    let mut first = RwLockWriteGuard::map(block_on(lock.write()), |pair| &mut pair.0);
    *first += 10;
    assert_eq!(format!("{first} {first:?}"), "11 11");
    assert!(lock.try_read().is_err());
    let first =
        RwLockMappedWriteGuard::try_map(first, |n| (*n > 100).then_some(n)).expect_err("small");
    let mut first = RwLockMappedWriteGuard::map(first, |n| n);
    *first += 1;
    drop(first);
    assert_eq!(lock.try_read().expect("free").0, 12);

    let guard = RwLockWriteGuard::try_map(block_on(lock.write()), |pair| {
        (pair.0 > 100).then_some(&mut pair.1)
    })
    .expect_err("small");
    assert!(lock.try_read().is_err(), "a declined map keeps the write");
    let whole = RwLockWriteGuard::into_mapped(guard);
    assert_eq!(whole.1, 2);
    drop(whole);

    let owned = block_on(Arc::clone(&lock).write_owned());
    let owned = OwnedRwLockWriteGuard::try_map(owned, |pair| (pair.0 > 100).then_some(&mut pair.0))
        .expect_err("small");
    let mut second = OwnedRwLockWriteGuard::map(owned, |pair| &mut pair.1);
    *second = 7;
    assert!(Arc::ptr_eq(
        OwnedRwLockMappedWriteGuard::rwlock(&second),
        &lock
    ));
    assert_eq!(format!("{second} {second:?}"), "7 7");
    let second = OwnedRwLockMappedWriteGuard::try_map(second, |n| (*n > 100).then_some(n))
        .expect_err("small");
    let second = OwnedRwLockMappedWriteGuard::map(second, |n| n);
    drop(second);
    let owned =
        OwnedRwLockWriteGuard::into_mapped(Arc::clone(&lock).try_write_owned().expect("free"));
    assert_eq!(owned.1, 7);
    drop(owned);
    assert!(lock.try_write().is_ok());
}

#[test]
fn max_readers_bounds_the_readers_inside() {
    let lock = Arc::new(RwLock::with_max_readers(0u8, 2));
    let a = lock.try_read().expect("first");
    let b = Arc::clone(&lock).try_read_owned().expect("second");
    assert!(lock.try_read().is_err(), "the third finds them all inside");
    assert!(
        lock.try_upgradable_read().is_some(),
        "an upgradable read is no reader"
    );
    let mut third = Polled::new(lock.read());
    third.pending();
    drop(a);
    assert_eq!(third.wakes(), 1, "a reader leaving wakes the waiting one");
    let third = third.ready();
    assert!(lock.try_read().is_err());
    drop((b, third));
    assert!(lock.try_write().is_ok());
}

#[test]
fn a_write_waits_for_all_the_readers_of_a_bounded_lock() {
    let lock = RwLock::with_max_readers((), 3);
    let readers: Vec<_> = (0..3).map(|_| lock.try_read().expect("room")).collect();
    let mut writer = Polled::new(lock.write());
    writer.pending();
    drop(readers);
    drop(writer.ready());
}

#[test]
fn a_static_lock_with_max_readers() {
    static LOCK: RwLock<u8> = RwLock::const_with_max_readers(0, 1);
    let only = LOCK.try_read().expect("first");
    assert!(LOCK.try_read().is_err());
    drop(only);
    assert!(LOCK.try_write().is_ok());
}

#[test]
#[should_panic(expected = "0 readers")]
fn a_lock_for_no_readers_is_refused() {
    let _ = RwLock::with_max_readers((), 0);
}

#[test]
#[should_panic(expected = "0 readers")]
fn a_static_lock_for_no_readers_is_refused() {
    let _ = RwLock::const_with_max_readers((), 0);
}

#[test]
#[should_panic(expected = "more than u32::MAX >> 3 readers")]
fn a_lock_for_more_readers_than_the_state_counts_is_refused() {
    let _ = RwLock::with_max_readers((), (u32::MAX >> 3) + 1);
}

#[test]
#[cfg(target_pointer_width = "64")]
fn the_most_readers_a_lock_can_be_built_for_is_accepted() {
    let lock = RwLock::with_max_readers(1, u32::MAX >> 3);
    assert_eq!(*lock.try_read().expect("room"), 1);
}

#[test]
fn dropped_rwlock_futures_pass_what_they_were_handed_on() {
    let lock = Arc::new(RwLock::new(()));
    let write = lock.try_write().expect("free");
    let mut reader = Polled::new(Arc::clone(&lock).read_owned());
    reader.pending();
    let mut writer = Polled::new(Arc::clone(&lock).write_owned());
    writer.pending();
    drop(write);
    assert_eq!((reader.wakes(), writer.wakes()), (1, 0));
    drop(reader);
    assert_eq!(writer.wakes(), 1, "the dropped wait woke the writer behind");
    drop(writer.ready());
    assert!(lock.try_write().is_ok(), "nothing was left held");

    let read = lock.try_read().expect("free");
    Polled::new(lock.write()).pending();
    drop(read);
    assert!(
        lock.try_write().is_ok(),
        "the dropped writer left nothing behind"
    );

    let mut owed = Polled::new(Arc::clone(&lock).write_owned());
    let mut inside = lock.try_read().expect("free");
    owed.pending();
    for _ in 0..MAX_BYPASS {
        let next = lock.try_read().expect("a read barges past the writer");
        drop(inside);
        inside = next;
        owed.pending();
    }
    let mut behind = Polled::new(Arc::clone(&lock).read_owned());
    behind.pending();
    drop(inside);
    drop(owed);
    assert_eq!(behind.wakes(), 1, "the handoff dropped unseen moved on");
    drop(behind.ready());
    assert!(lock.try_write().is_ok());
}

#[test]
fn unsized_values_lock_too() {
    let mutex: Arc<Mutex<dyn std::fmt::Debug + Send>> = Arc::new(Mutex::new(5u8));
    let guard = block_on(Arc::clone(&mutex).lock_owned());
    assert_eq!(format!("{guard:?}"), "5");
    assert!(mutex.try_lock().is_err());
    drop(guard);
    assert_eq!(format!("{:?}", &*mutex.try_lock().expect("free")), "5");

    let lock: Arc<RwLock<[u8]>> = Arc::new(RwLock::new([1, 2, 3]));
    let first = OwnedRwLockReadGuard::map(block_on(Arc::clone(&lock).read_owned()), |all| &all[0]);
    assert_eq!(*first, 1);
    drop(first);
    let mut write = block_on(Arc::clone(&lock).write_owned());
    write.reverse();
    let read = write.downgrade();
    assert_eq!(&*read, [3, 2, 1]);
}
