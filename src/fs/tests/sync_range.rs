//! Writing a range of a file back: the writes it covers, the sizes around the
//! ring entry's length field, the ring operation, and the failures that stay.

use super::*;

fn open_rw(path: &PathBuf) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

#[test]
fn a_range_sync_covers_the_writes_accepted_before_it() {
    let rt = runtime();
    let tmp = Temp::new("range-covers");
    let path = tmp.file("data");
    let data = pattern(3 << 20 | 17);
    let mut expected = data.clone();
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, &data)).await.unwrap();
        within(file.write_at(b"positional", 5)).await.unwrap();
        within(file.sync_range(0, 0)).await.unwrap();
        assert!(
            !file.file().unwrap().has_unsent(),
            "the sync left writes behind"
        );
    });
    expected[5..15].copy_from_slice(b"positional");
    assert!(std::fs::read(&path).unwrap() == expected);
}

#[test]
fn a_range_sync_answers_for_every_range() {
    let rt = runtime();
    let tmp = Temp::new("range-sizes");
    let path = tmp.file("data");
    std::fs::write(&path, pattern(1 << 20)).unwrap();
    let wide = u64::from(u32::MAX);
    let ranges = [
        (0, 0),
        (0, 1),
        (4096, 4096),
        (10, 70_000),
        (0, 2 << 20),
        (5 << 20, 10),
        (0, wide),
        (0, wide + 1),
        (123, 1 << 33),
    ];
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        for (offset, len) in ranges {
            within(file.sync_range(offset, len))
                .await
                .unwrap_or_else(|err| panic!("{offset} {len}: {err:?}"));
        }
    });
}

#[test]
fn a_range_sync_reports_a_failed_write() {
    let rt = runtime();
    let tmp = Temp::new("range-failed-write");
    let path = tmp.file("data");
    std::fs::write(&path, b"read only").unwrap();
    rt.block_on(async {
        let mut file = within(Handle::<File>::open(&path)).await.unwrap();
        let err = match within(write_all(&mut file, b"nope")).await {
            Err(err) => err,
            Ok(()) => within(file.sync_range(0, 0)).await.unwrap_err(),
        };
        assert!(err.raw_os_error().is_some(), "{err:?}");
    });
}

#[cfg(target_os = "linux")]
#[test]
fn a_failed_sync_fails_a_range_sync_too_without_a_call() {
    let rt = super::durable::one_blocking_thread_kept();
    rt.block_on(async {
        within(spawn(async move {
            let file = within(Handle::<File>::open("/proc/self/status"))
                .await
                .unwrap();
            let first = within(file.sync_all()).await.unwrap_err();
            assert_eq!(first.raw_os_error(), Some(libc::EINVAL), "{first:?}");
            let before = super::durable::ring_calls().map(|(n, _)| n);
            let later = within(file.sync_range(0, 0)).await.unwrap_err();
            assert_eq!(later.raw_os_error(), Some(libc::EINVAL));
            assert_eq!(
                super::durable::ring_calls().map(|(n, _)| n),
                before,
                "a later sync made a call"
            );
        }))
        .await;
    });
}

#[cfg(target_os = "linux")]
#[test]
fn a_stream_gets_the_kernels_error_from_a_range_sync() {
    use std::os::fd::OwnedFd;
    let rt = runtime();
    let (r, _w) = std::io::pipe().unwrap();
    rt.block_on(async {
        let reader = Handle::new(File::from(OwnedFd::from(r))).unwrap();
        let err = within(reader.sync_range(0, 0)).await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ESPIPE));
    });
}

/// With the one thread of the blocking pool held, a range sync still
/// completes: it is `IORING_OP_SYNC_FILE_RANGE`, with the range and the three
/// flags; a range its length field cannot hold goes to the pool instead.
#[cfg(ring)]
#[test]
fn a_range_sync_is_an_operation_of_the_ring() {
    use super::owned::hold_the_pool;
    use crate::syscore::linux::iouring::ring::sys::Opcode;
    let rt = super::durable::one_blocking_thread_kept();
    let tmp = Temp::new("range-ring");
    let path = tmp.file("data");
    rt.block_on(async {
        within(spawn(async move {
            if super::durable::ring_calls().is_none() {
                eprintln!("skipping: the shard runs no ring (NUCLEI_BACKEND or no io_uring)");
                return;
            }
            let mut file = Handle::new(read_write(&path)).unwrap();
            within(write_all(&mut file, b"durable")).await.unwrap();
            within(flush(&mut file)).await.unwrap();
            let _ = super::durable::ring_calls();
            let (hold, busy) = hold_the_pool();
            within(file.sync_range(4096, 8192)).await.unwrap();
            within(file.sync_range(0, 0)).await.unwrap();
            let (_, entries) = super::durable::ring_calls().unwrap();
            let ranges: Vec<_> = entries
                .iter()
                .filter(|e| e.0 == Opcode::SyncFileRange as u8)
                .map(|e| (e.1, e.2, e.3))
                .collect();
            assert_eq!(ranges, [(8192, 4096, 7), (0, 0, 7)]);
            hold.send(()).unwrap();
            within(busy).await;
            within(file.sync_range(0, u64::from(u32::MAX) + 1))
                .await
                .unwrap();
            let (_, entries) = super::durable::ring_calls().unwrap();
            assert!(
                entries.iter().all(|e| e.0 != Opcode::SyncFileRange as u8),
                "a range past the entry's length field went to the ring"
            );
        }))
        .await;
    });
}
