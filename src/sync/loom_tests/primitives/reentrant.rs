//! The reentrant locks: depth, a reader behind a waiting writer and a write that becomes a read.

use super::*;

#[test]
fn a_reentrant_mutex_unwinds_its_depth_before_another_owner_enters() {
    explore(
        "a_reentrant_mutex_unwinds_its_depth_before_another_owner_enters",
        3,
        5,
        || {
            let lock = Arc::new(ReentrantMutex::new(UnsafeCell::new(0usize)));
            let other = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    let owner = Owner::new();
                    bump(&block_on(lock.lock(&owner)));
                })
            };
            let owner = Owner::new();
            let outer = block_on(lock.lock(&owner));
            let inner = block_on(lock.lock(&owner));
            bump(&inner);
            drop(inner);
            bump(&outer);
            drop(outer);
            other.join().expect("other owner");
            assert_eq!(read(&lock.try_lock(&owner).expect("free")), 3);
        },
    );
}

#[test]
fn a_reentrant_reader_never_waits_behind_a_writer_waiting_for_it() {
    explore(
        "a_reentrant_reader_never_waits_behind_a_writer_waiting_for_it",
        3,
        5,
        || {
            let lock = Arc::new(ReentrantRwLock::new(AtomicUsize::new(0)));
            let owner = Owner::new();
            let first = lock.try_read(&owner).expect("free");
            let writer = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    let owner = Owner::new();
                    block_on(lock.write(&owner))
                        .expect("this owner holds no read")
                        .fetch_add(1, Ordering::Relaxed);
                })
            };
            let second = block_on(lock.read(&owner));
            drop(first);
            drop(second);
            writer.join().expect("writer");
            assert_eq!(
                lock.try_read(&owner).expect("free").load(Ordering::Relaxed),
                1
            );
        },
    );
}

#[test]
fn a_reentrant_write_becomes_its_owners_read_while_another_owner_waits() {
    explore(
        "a_reentrant_write_becomes_its_owners_read_while_another_owner_waits",
        3,
        5,
        || {
            let lock = Arc::new(ReentrantRwLock::new(AtomicUsize::new(0)));
            let owner = Owner::new();
            let write = lock.try_write(&owner).expect("free");
            let read = lock.try_read(&owner).expect("a writer may read");
            let other = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    let owner = Owner::new();
                    block_on(lock.write(&owner))
                        .expect("this owner holds no read")
                        .fetch_add(1, Ordering::Relaxed);
                })
            };
            write.fetch_add(1, Ordering::Relaxed);
            drop(write);
            assert_eq!(
                read.load(Ordering::Relaxed),
                1,
                "the read outlives the write"
            );
            drop(read);
            other.join().expect("other owner");
            assert_eq!(
                lock.try_write(&Owner::new())
                    .expect("free")
                    .load(Ordering::Relaxed),
                2
            );
        },
    );
}
