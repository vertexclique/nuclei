//! Syncing a file: on the shard's ring where it runs one, after every write
//! the sync covers, and a failure that stays.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Waker};

use super::lent_wake::{Flag, woken};
use super::owned::{hold_the_pool, one_blocking_thread};
use super::*;

/// Polls `fut` once with a waker of its own: it must wait. The flag says when
/// it was woken.
fn pend_flagged<F: Future + Unpin>(fut: &mut F, what: &str) -> Arc<Flag> {
    let flag = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&flag));
    let polled = Pin::new(fut).poll(&mut Context::from_waker(&waker));
    assert!(polled.is_pending(), "{what} did not wait");
    flag
}

/// A runtime of one worker and one blocking thread whose shard never goes
/// to another thread, so a call made from a task's poll always finds the
/// worker's ring (handed off, the poll's thread runs no shard, and its calls
/// go to the blocking pool, as they must).
#[cfg(target_os = "linux")]
pub(super) fn one_blocking_thread_kept() -> Runtime {
    Builder::new()
        .worker_threads(1)
        .max_blocking_threads(1)
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .unwrap()
}

/// A file's call on the ring: its opcode, length, offset and flags.
pub(super) type Entry = (u8, u32, u64, u32);

/// The ring of this thread's shard: its file calls so far, and the
/// opcode, length, offset and flags of each since the last look.
#[cfg(ring)]
pub(super) fn ring_calls() -> Option<(usize, Vec<Entry>)> {
    crate::rt::context::current(|shard| {
        shard
            .and_then(|s| s.ring())
            .map(|ring| (ring.file_calls(), ring.take_file_entries()))
    })
}

#[cfg(not(ring))]
pub(super) fn ring_calls() -> Option<(usize, Vec<Entry>)> {
    None
}

/// With the one thread of the blocking pool held, a sync still completes:
/// it is an operation of the ring, `IORING_OP_FSYNC`, data-only for
/// `sync_data`.
#[cfg(ring)]
#[test]
fn a_sync_is_an_operation_of_the_ring() {
    use crate::syscore::linux::iouring::ring::sys::{IORING_FSYNC_DATASYNC, Opcode};
    let rt = one_blocking_thread_kept();
    let tmp = Temp::new("sync-ring");
    let path = tmp.file("data");
    rt.block_on(async {
        within(spawn(async move {
            if ring_calls().is_none() {
                eprintln!("skipping: the shard runs no ring (NUCLEI_BACKEND or no io_uring)");
                return;
            }
            let mut file = Handle::new(read_write(&path)).unwrap();
            within(write_all(&mut file, b"durable")).await.unwrap();
            within(flush(&mut file)).await.unwrap();
            let _ = ring_calls();
            let (hold, busy) = hold_the_pool();
            within(file.sync_all()).await.unwrap();
            within(file.sync_data()).await.unwrap();
            let (_, entries) = ring_calls().unwrap();
            let syncs: Vec<_> = entries
                .iter()
                .filter(|e| e.0 == Opcode::Fsync as u8)
                .map(|e| e.3)
                .collect();
            assert_eq!(
                syncs,
                [0, IORING_FSYNC_DATASYNC],
                "the syncs were not the ring's"
            );
            hold.send(()).unwrap();
            within(busy).await;
        }))
        .await;
    });
}

/// Bytes accepted and not yet written when the sync is called are written
/// before it.
#[test]
fn a_sync_covers_the_writes_accepted_before_it() {
    let rt = runtime();
    let tmp = Temp::new("sync-covers");
    let path = tmp.file("data");
    let data = pattern(3 << 20 | 17);
    let expected = data.clone();
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, &data)).await.unwrap();
        within(file.write_at(b"positional", 5)).await.unwrap();
        within(file.sync_data()).await.unwrap();
        assert!(
            !file.file().unwrap().has_unsent(),
            "the sync left writes behind"
        );
        within(file.sync_all()).await.unwrap();
    });
    let mut expected = expected;
    expected[5..15].copy_from_slice(b"positional");
    assert!(std::fs::read(&path).unwrap() == expected);
}

/// A write that failed is reported by the sync, as by a flush.
#[test]
fn a_sync_reports_a_failed_write() {
    let rt = runtime();
    let tmp = Temp::new("sync-failed-write");
    let path = tmp.file("data");
    std::fs::write(&path, b"read only").unwrap();
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        let err = match within(write_all(&mut file, b"nope")).await {
            Err(err) => err,
            Ok(()) => within(file.sync_all()).await.unwrap_err(),
        };
        assert!(err.raw_os_error().is_some(), "{err:?}");
    });
}

/// A sync the kernel fails (procfs has no sync) is reported, and every later
/// sync of the handle fails with its error, without a call: not on the
/// ring, not on the pool (whose one thread is held).
#[cfg(target_os = "linux")]
#[test]
fn a_failed_sync_fails_every_later_one() {
    let rt = one_blocking_thread_kept();
    rt.block_on(async {
        within(spawn(async move {
            let file = within(Handle::<File>::open("/proc/self/status"))
                .await
                .unwrap();
            let first = within(file.sync_all()).await.unwrap_err();
            assert_eq!(first.raw_os_error(), Some(libc::EINVAL), "{first:?}");
            let before = ring_calls().map(|(n, _)| n);
            let (hold, busy) = hold_the_pool();
            for later in [
                within(file.sync_data()).await,
                within(file.sync_all()).await,
            ] {
                assert_eq!(later.unwrap_err().raw_os_error(), Some(libc::EINVAL));
            }
            assert_eq!(
                ring_calls().map(|(n, _)| n),
                before,
                "a later sync made a call"
            );
            hold.send(()).unwrap();
            within(busy).await;
        }))
        .await;
    });
}

/// A sync whose future went away (a timeout dropped it) after its call failed
/// leaves that failure to the next sync, which fails with it: the system
/// reports a lost write-back once, to the call that failed, so a call made
/// after it would answer that all is well.
#[test]
fn a_sync_dropped_after_its_call_failed_fails_the_next_one() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("sync-dropped");
    let path = tmp.file("data");
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, b"durable")).await.unwrap();
        within(flush(&mut file)).await.unwrap();
        let io = file.file().unwrap();
        io.keep_on_pool();
        io.fail_next_syncs(1);
        let (release, busy) = hold_the_pool();
        let mut dropped = Box::pin(file.sync_all());
        let flag = pend_flagged(&mut dropped, "the sync on the held pool");
        release.send(()).unwrap();
        within(busy).await;
        woken(&flag, "the sync's call never ended").await;
        drop(dropped);
        for later in [
            within(file.sync_data()).await,
            within(file.sync_all()).await,
        ] {
            let err = later.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Other, "{err:?}");
        }
    });
}

/// Two syncs at once make one call at a time: the second waits for the
/// first's call, and once that failed, fails too, though its task looks
/// first. Two calls beside each other would let the kernel report the lost
/// write-back to one only, and the other answer that all is well.
#[test]
fn a_sync_beside_one_whose_call_fails_fails_too() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("sync-beside");
    let path = tmp.file("data");
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, b"durable")).await.unwrap();
        within(flush(&mut file)).await.unwrap();
        let io = file.file().unwrap();
        io.keep_on_pool();
        io.fail_next_syncs(1);
        let (release, busy) = hold_the_pool();
        let mut first = Box::pin(file.sync_all());
        let mut second = Box::pin(file.sync_data());
        let _ = pend_flagged(&mut first, "the first sync on the held pool");
        let woke = pend_flagged(&mut second, "the second sync on the held pool");
        release.send(()).unwrap();
        within(busy).await;
        woken(&woke, "the second sync was never woken").await;
        let err = within(second).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other, "{err:?}");
        let err = within(first).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other, "{err:?}");
    });
}

/// On the ring as on the pool: a sync dropped after its call failed (procfs
/// has no sync) leaves its failure, and the next sync answers it with no call
/// of its own.
#[cfg(ring)]
#[test]
fn a_sync_dropped_after_its_ring_call_failed_fails_the_next_with_no_call() {
    let rt = one_blocking_thread_kept();
    rt.block_on(async {
        within(spawn(async move {
            if ring_calls().is_none() {
                eprintln!("skipping: the shard runs no ring (NUCLEI_BACKEND or no io_uring)");
                return;
            }
            let file = within(Handle::<File>::open("/proc/self/status"))
                .await
                .unwrap();
            let mut dropped = Box::pin(file.sync_all());
            let flag = pend_flagged(&mut dropped, "the sync on the ring");
            woken(&flag, "the sync's ring call never ended").await;
            drop(dropped);
            let before = ring_calls().map(|(n, _)| n);
            let (hold, busy) = hold_the_pool();
            let err = within(file.sync_data()).await.unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "{err:?}");
            assert_eq!(
                ring_calls().map(|(n, _)| n),
                before,
                "the next sync made a call of its own"
            );
            hold.send(()).unwrap();
            within(busy).await;
        }))
        .await;
    });
}

/// A write the handle accepted, which the kernel then fails (the file is open
/// for reading only), is reported once by a flush, which forgets it; every
/// sync after that still fails, with no call: the kernel has nothing to say
/// of bytes it never took.
#[test]
fn a_write_lost_after_it_was_accepted_fails_every_later_sync() {
    let rt = runtime();
    let tmp = Temp::new("sync-lost-write");
    let path = tmp.file("data");
    std::fs::write(&path, b"read only").unwrap();
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        within(write_all(&mut file, b"lost")).await.unwrap();
        within(flush(&mut file)).await.unwrap_err();
        within(flush(&mut file)).await.unwrap();
        for (what, later) in [
            ("sync_all", within(file.sync_all()).await),
            ("sync_data", within(file.sync_data()).await),
            ("sync_range", within(file.sync_range(0, 0)).await),
        ] {
            assert!(later.is_err(), "{what} answered success after a lost write");
        }
    });
}

/// A lost write taken by a call beside the reads and writes (`metadata`), or by
/// `get_mut`, which ends the handle's IO state, still fails every later sync.
#[test]
fn a_lost_write_taken_by_another_call_still_fails_the_syncs() {
    let rt = runtime();
    let tmp = Temp::new("sync-lost-taken");
    let path = tmp.file("data");
    std::fs::write(&path, b"read only").unwrap();
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        within(write_all(&mut file, b"lost")).await.unwrap();
        within(file.metadata()).await.unwrap_err();
        assert!(
            within(file.sync_all()).await.is_err(),
            "a sync answered success after metadata took a lost write"
        );

        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        within(write_all(&mut file, b"lost")).await.unwrap();
        let _ = file.get_mut();
        assert!(
            within(file.sync_data()).await.is_err(),
            "a sync answered success after get_mut took a lost write"
        );
    });
}

/// A clone shares what the handle's syncs answer: after a write the handle
/// lost, and after a sync of the handle failed, the clone's syncs fail too
/// (the system reports a lost write-back of the open file to one call only).
#[test]
fn a_clone_fails_its_syncs_for_the_handle_s_lost_write_and_failed_sync() {
    let rt = runtime();
    let tmp = Temp::new("sync-clone");
    let read_only = tmp.file("read-only");
    std::fs::write(&read_only, b"read only").unwrap();
    let path = tmp.file("data");
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&read_only)).await.unwrap();
        let clone = within(file.try_clone()).await.unwrap();
        within(write_all(&mut file, b"lost")).await.unwrap();
        within(flush(&mut file)).await.unwrap_err();
        assert!(
            within(clone.sync_all()).await.is_err(),
            "the clone's sync answered success after the handle lost a write"
        );

        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, b"durable")).await.unwrap();
        let clone = within(file.try_clone()).await.unwrap();
        let io = file.file().unwrap();
        io.keep_on_pool();
        io.fail_next_syncs(1);
        let err = within(file.sync_all()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other, "{err:?}");
        let err = within(clone.sync_data()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other, "{err:?}");
    });
}

/// On a ring made to poll for its completions (`IORING_SETUP_IOPOLL`), which
/// refuses a write and a read of a buffered file, a sync, a range's write-back
/// and a change of a range's space, each refused call is made again on the
/// blocking pool: every byte lands and reads back, and every sync succeeds.
/// io_uring only: no other backend has such a ring.
#[cfg(ring)]
#[test]
fn a_polled_ring_makes_the_calls_it_refuses_on_the_pool() {
    let built = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(None)
        .io_uring(crate::config::IoUringConfiguration::io_poll(256))
        .build();
    let rt = match built {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("skipping: no runtime on a polled ring here ({err})");
            return;
        }
    };
    let tmp = Temp::new("sync-iopoll");
    let path = tmp.file("data");
    let data = pattern(200_017);
    let expected = data.clone();
    let read_path = path.clone();
    let ran = rt.block_on(async {
        within(spawn(async move {
            if ring_calls().is_none() {
                eprintln!("skipping: the shard runs no ring (NUCLEI_BACKEND or no io_uring)");
                return false;
            }
            let mut file = Handle::new(read_write(&path)).unwrap();
            within(write_all(&mut file, &data)).await.unwrap();
            within(flush(&mut file)).await.unwrap();
            within(file.allocate(0, 1 << 20, crate::fs::Allocate::KeepSize))
                .await
                .unwrap();
            within(file.sync_range(0, 4096)).await.unwrap();
            within(file.sync_data()).await.unwrap();
            within(file.sync_all()).await.unwrap();
            within(seek(&mut file, SeekFrom::Start(0))).await.unwrap();
            let back = within(read_to_end(&mut file)).await.unwrap();
            assert!(back == data, "the bytes read back are not the ones written");
            let (_, entries) = ring_calls().unwrap();
            assert!(!entries.is_empty(), "no call went to the polled ring");
            true
        }))
        .await
    });
    if ran {
        assert!(std::fs::read(&read_path).unwrap() == expected);
    }
}
