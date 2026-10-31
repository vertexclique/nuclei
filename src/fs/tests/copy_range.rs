//! Copying a range of one file into another: sizes around the chunk, the end
//! of the source, overlapping ranges of one file, ordering with the writes
//! of both files, a dropped copy, and the failures the kernel reports.

use super::owned::{hold_the_pool, one_blocking_thread, pends};
use super::*;
use crate::fs::file::COPY_CHUNK;

const CHUNK: usize = COPY_CHUNK as usize;
const BUFFER: usize = 64 * 1024;

fn open_rw(path: &PathBuf) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

fn long_pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(131) ^ (i >> 12) as u8 ^ (i >> 20) as u8)
        .collect()
}

#[test]
fn a_range_is_copied_whole_for_every_size() {
    let rt = runtime();
    let tmp = Temp::new("copy-sizes");
    let (src, dst) = (tmp.file("src"), tmp.file("dst"));
    let sizes = [
        1,
        4095,
        4096,
        BUFFER - 1,
        BUFFER,
        BUFFER + 1,
        3 * BUFFER + 5,
        CHUNK - 1,
        CHUNK,
        CHUNK + 1,
    ];
    let data = long_pattern(CHUNK + 1 + 200);
    std::fs::write(&src, &data).unwrap();
    rt.block_on(async {
        let from = Handle::new(open_rw(&src)).unwrap();
        for len in sizes {
            let to = Handle::new(read_write(&dst)).unwrap();
            let copied = within(from.copy_range_to(&to, 7, 13, len as u64)).await;
            assert_eq!(copied.unwrap(), len as u64, "{len}");
            drop(to);
            let got = std::fs::read(&dst).unwrap();
            assert_eq!(got.len(), 13 + len, "{len}");
            assert!(got[..13].iter().all(|&b| b == 0), "{len}");
            assert!(got[13..] == data[7..7 + len], "{len} bytes changed");
        }
    });
}

#[test]
fn the_end_of_the_source_ends_the_copy_inside_a_chunk() {
    let rt = runtime();
    let tmp = Temp::new("copy-end");
    let (src, dst) = (tmp.file("src"), tmp.file("dst"));
    let data = long_pattern(CHUNK + 1000);
    std::fs::write(&src, &data).unwrap();
    rt.block_on(async {
        let from = Handle::new(open_rw(&src)).unwrap();
        let to = Handle::new(read_write(&dst)).unwrap();
        let asked = 2 * COPY_CHUNK;
        assert_eq!(
            within(from.copy_range_to(&to, 0, 0, asked)).await.unwrap(),
            data.len() as u64
        );
        let end = data.len() as u64;
        assert_eq!(
            within(from.copy_range_to(&to, end, 0, 10)).await.unwrap(),
            0
        );
        assert_eq!(
            within(from.copy_range_to(&to, end + 5000, 0, 10))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            within(from.copy_range_to(&to, end - 50, 0, 100))
                .await
                .unwrap(),
            50
        );
    });
    let got = std::fs::read(&dst).unwrap();
    assert!(
        got[..50] == data[data.len() - 50..],
        "the last bytes changed"
    );
    assert!(
        got[50..] == data[50..],
        "an empty copy changed the destination"
    );
}

#[test]
fn an_empty_copy_copies_nothing() {
    let rt = runtime();
    let tmp = Temp::new("copy-empty");
    let (src, dst) = (tmp.file("src"), tmp.file("dst"));
    std::fs::write(&src, pattern(100)).unwrap();
    std::fs::write(&dst, b"kept").unwrap();
    rt.block_on(async {
        let from = Handle::new(open_rw(&src)).unwrap();
        let to = Handle::new(open_rw(&dst)).unwrap();
        assert_eq!(within(from.copy_range_to(&to, 0, 0, 0)).await.unwrap(), 0);
    });
    assert_eq!(std::fs::read(&dst).unwrap(), b"kept");
}

#[test]
fn a_range_past_what_a_file_can_hold_is_invalid_input() {
    let rt = runtime();
    let tmp = Temp::new("copy-overflow");
    let (src, dst) = (tmp.file("src"), tmp.file("dst"));
    std::fs::write(&src, pattern(100)).unwrap();
    std::fs::write(&dst, b"kept").unwrap();
    rt.block_on(async {
        let from = Handle::new(open_rw(&src)).unwrap();
        let to = Handle::new(open_rw(&dst)).unwrap();
        let top = i64::MAX as u64;
        for (a, b, len) in [(u64::MAX, 0, 1), (0, top, 1), (top, 0, 1), (1, 1, u64::MAX)] {
            let err = within(from.copy_range_to(&to, a, b, len))
                .await
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{a} {b} {len}");
        }
    });
    assert_eq!(std::fs::read(&dst).unwrap(), b"kept");
}

#[test]
fn overlapping_ranges_of_one_file_are_refused_before_any_copy() {
    let rt = runtime();
    let tmp = Temp::new("copy-overlap");
    let path = tmp.file("data");
    let linked = tmp.file("linked");
    let data = pattern(1000);
    std::fs::write(&path, &data).unwrap();
    std::fs::hard_link(&path, &linked).unwrap();
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        let clone = within(file.try_clone()).await.unwrap();
        let other_name = Handle::new(open_rw(&linked)).unwrap();
        for to in [&file, &clone, &other_name] {
            for (from_at, to_at) in [(0, 50), (50, 0), (0, 0), (10, 99), (99, 10)] {
                let err = within(file.copy_range_to(to, from_at, to_at, 100))
                    .await
                    .unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{from_at} {to_at}");
                assert_eq!(err.to_string(), "the ranges overlap in one file");
            }
        }
    });
    assert!(std::fs::read(&path).unwrap() == data);
}

#[test]
fn ranges_of_one_file_that_do_not_overlap_are_copied() {
    let rt = runtime();
    let tmp = Temp::new("copy-disjoint");
    let path = tmp.file("data");
    let data = long_pattern(300_000);
    std::fs::write(&path, &data).unwrap();
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        let clone = within(file.try_clone()).await.unwrap();
        assert_eq!(
            within(file.copy_range_to(&clone, 0, 100_000, 100_000))
                .await
                .unwrap(),
            100_000
        );
        assert_eq!(
            within(file.copy_range_to(&file, 0, 200_000, 100))
                .await
                .unwrap(),
            100
        );
        assert_eq!(
            within(file.copy_range_to(&file, 0, 500_000, 0))
                .await
                .unwrap(),
            0
        );
    });
    let mut expected = data.clone();
    expected.copy_within(0..100_000, 100_000);
    expected[200_000..200_100].copy_from_slice(&data[..100]);
    assert!(std::fs::read(&path).unwrap() == expected);
}

#[test]
fn a_destination_that_cannot_be_written_gets_the_kernels_error() {
    let rt = runtime();
    let tmp = Temp::new("copy-read-only");
    let (src, dst) = (tmp.file("src"), tmp.file("dst"));
    std::fs::write(&src, pattern(5000)).unwrap();
    std::fs::write(&dst, b"kept").unwrap();
    rt.block_on(async {
        let from = Handle::new(open_rw(&src)).unwrap();
        let read_only = within(Handle::<File>::open(&dst)).await.unwrap();
        let err = within(from.copy_range_to(&read_only, 0, 0, 5000))
            .await
            .unwrap_err();
        assert!(err.raw_os_error().is_some(), "{err:?}");
        #[cfg(unix)]
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
        let unreadable = Handle::new(OpenOptions::new().write(true).open(&src).unwrap()).unwrap();
        let to = Handle::new(open_rw(&dst)).unwrap();
        let err = within(unreadable.copy_range_to(&to, 0, 0, 10))
            .await
            .unwrap_err();
        assert!(err.raw_os_error().is_some(), "{err:?}");
        #[cfg(unix)]
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
    });
    assert_eq!(std::fs::read(&dst).unwrap(), b"kept");
}

/// A destination opened to append refuses the kernel's copy (`EBADF`), so
/// the bytes go through the pool's buffers and land at its end.
#[cfg(unix)]
#[test]
fn a_destination_the_kernel_cannot_copy_into_is_copied_through_a_buffer() {
    let rt = runtime();
    let tmp = Temp::new("copy-fallback");
    let (src, dst) = (tmp.file("src"), tmp.file("dst"));
    let data = long_pattern(3 * BUFFER + 77);
    std::fs::write(&src, &data).unwrap();
    std::fs::write(&dst, b"").unwrap();
    rt.block_on(async {
        let from = Handle::new(open_rw(&src)).unwrap();
        let to = Handle::new(OpenOptions::new().append(true).open(&dst).unwrap()).unwrap();
        let copied = within(from.copy_range_to(&to, 0, 0, data.len() as u64)).await;
        assert_eq!(copied.unwrap(), data.len() as u64);
    });
    assert!(std::fs::read(&dst).unwrap() == data);
}

#[test]
fn a_copy_waits_for_the_writes_accepted_on_both_files_before_it() {
    let rt = runtime();
    let tmp = Temp::new("copy-order");
    let (src, dst) = (tmp.file("src"), tmp.file("dst"));
    rt.block_on(async {
        let mut from = Handle::new(read_write(&src)).unwrap();
        let mut to = Handle::new(read_write(&dst)).unwrap();
        within(write_all(&mut from, &[b's'; 20_000])).await.unwrap();
        within(write_all(&mut to, &[b'd'; 20_000])).await.unwrap();
        let copied = within(from.copy_range_to(&to, 0, 0, 10_000)).await.unwrap();
        assert_eq!(copied, 10_000);
        within(flush(&mut to)).await.unwrap();
    });
    let got = std::fs::read(&dst).unwrap();
    assert!(
        got[..10_000].iter().all(|&b| b == b's'),
        "the copy landed before a write"
    );
    assert!(got[10_000..].iter().all(|&b| b == b'd'));
}

#[test]
fn a_cursor_read_after_a_copy_sees_the_copied_bytes() {
    let rt = runtime();
    let tmp = Temp::new("copy-generation");
    let (src, dst) = (tmp.file("src"), tmp.file("dst"));
    std::fs::write(&src, vec![b'n'; 200_000]).unwrap();
    std::fs::write(&dst, vec![b'o'; 200_000]).unwrap();
    rt.block_on(async {
        let from = Handle::new(open_rw(&src)).unwrap();
        let mut to = Handle::new(open_rw(&dst)).unwrap();
        let mut head = [0u8; 10];
        assert!(within(read_some(&mut to, &mut head)).await.unwrap() > 0);
        within(from.copy_range_to(&to, 0, 0, 100_000))
            .await
            .unwrap();
        let mut next = [0u8; 64];
        let n = within(read_some(&mut to, &mut next)).await.unwrap();
        assert!(
            n > 0 && next[..n].iter().all(|&b| b == b'n'),
            "stale bytes were handed out"
        );
    });
}

#[test]
fn a_dropped_copy_leaves_its_chunk_to_the_destination_and_makes_no_more() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("copy-dropped");
    let (src, dst) = (tmp.file("src"), tmp.file("dst"));
    let data = long_pattern(CHUNK + 1);
    std::fs::write(&src, &data).unwrap();
    rt.block_on(async {
        let from = Handle::new(open_rw(&src)).unwrap();
        let to = Handle::new(read_write(&dst)).unwrap();
        let (hold, busy) = hold_the_pool();
        let mut dropped = Box::pin(from.copy_range_to(&to, 0, 0, data.len() as u64));
        pends(&mut dropped).await;
        drop(dropped);
        let mut buf = [0u8; 4];
        let mut after = Box::pin(to.read_at(&mut buf, 0));
        pends(&mut after).await;
        hold.send(()).unwrap();
        within(busy).await;
        assert_eq!(within(after).await.unwrap(), 4);
    });
    let got = std::fs::read(&dst).unwrap();
    assert!(
        !got.is_empty() && got.len() < data.len() && got == data[..got.len()],
        "the chunk that was out did not land, or a chunk after it did"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_stream_gets_the_kernels_answer() {
    use std::os::fd::{AsRawFd, OwnedFd};
    let rt = runtime();
    let tmp = Temp::new("copy-stream");
    let dst = tmp.file("dst");
    let (r, _w) = std::io::pipe().unwrap();
    let target = read_write(&dst);
    let (mut off_in, mut off_out) = (0, 0);
    // SAFETY: a plain syscall on open descriptors and our own offsets.
    let got = unsafe {
        libc::copy_file_range(
            r.as_raw_fd(),
            &mut off_in,
            target.as_raw_fd(),
            &mut off_out,
            4,
            0,
        )
    };
    let theirs = std::io::Error::last_os_error().raw_os_error();
    assert_eq!(got, -1);
    rt.block_on(async {
        let from = Handle::new(File::from(OwnedFd::from(r))).unwrap();
        let to = Handle::new(open_rw(&dst)).unwrap();
        let err = within(from.copy_range_to(&to, 0, 0, 4)).await.unwrap_err();
        assert_eq!(err.raw_os_error(), theirs);
    });
}
