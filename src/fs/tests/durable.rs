//! Syncing a file: on the shard's ring where it runs one, after every write
//! the sync covers, and a failure that stays.

#[cfg(target_os = "linux")]
use super::owned::hold_the_pool;
use super::*;

/// A runtime of one worker and one blocking thread whose shard never goes
/// to another thread, so a call made from a task's poll always finds the
/// worker's ring (handed off, the poll's thread runs no shard, and its calls
/// go to the blocking pool, as they must).
#[cfg(target_os = "linux")]
fn one_blocking_thread_kept() -> Runtime {
    Builder::new()
        .worker_threads(1)
        .max_blocking_threads(1)
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .unwrap()
}

/// A file's call on the ring: its opcode, length, offset and flags.
#[cfg(target_os = "linux")]
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

#[cfg(all(not(ring), target_os = "linux"))]
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
        within(file.sync_data()).await.unwrap();
        assert!(
            !file.file().unwrap().has_unsent(),
            "the sync left writes behind"
        );
        within(file.sync_all()).await.unwrap();
    });
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
