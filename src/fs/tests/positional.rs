//! Reads and writes at offsets of the caller's own: every size the buffers'
//! thresholds meet, many tasks on one handle, and the rules they keep with
//! the calls at the cursor.

use std::sync::Arc;

use super::owned::{hold_the_pool, one_blocking_thread, pends};
use super::*;

const SIZES: [usize; 10] = [
    0,
    1,
    4095,
    4096,
    4097,
    65535,
    65536,
    65537,
    1 << 20,
    64 << 20,
];

async fn long<F: Future>(fut: F) -> F::Output {
    crate::time::timeout(Duration::from_secs(120), fut)
        .await
        .expect("the operation never completed")
}

/// Whether this thread's shard runs a ring.
fn on_a_ring() -> bool {
    super::durable::ring_calls().is_some()
}

fn seeded(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ (i / 4093) as u8)
        .collect()
}

#[test]
fn positional_calls_move_every_size_and_leave_the_cursor() {
    let rt = runtime();
    let tmp = Temp::new("pos-sizes");
    let path = tmp.file("data");
    let written = rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, b"head")).await.unwrap();
        let mut at = 16u64;
        let mut placed = Vec::new();
        for (i, &size) in SIZES.iter().enumerate() {
            let data = seeded(size, i as u8);
            long(file.write_all_at(&data, at)).await.unwrap();
            let mut got = vec![0u8; size];
            long(file.read_exact_at(&mut got, at)).await.unwrap();
            assert!(got == data, "{size} bytes at {at} came back changed");
            let mut owned = Vec::with_capacity(size);
            while owned.len() < size {
                let from = at + owned.len() as u64;
                let (n, back) = long(file.read_at_owned(owned, from)).await;
                owned = back;
                assert!(n.unwrap() > 0, "{size} bytes at {at} ended early");
            }
            assert!(
                owned == data,
                "{size} bytes read into a lent buffer changed"
            );
            let again = seeded(size, i as u8 ^ 0x5a);
            let (n, back) = long(file.write_at_owned(again.clone(), at)).await;
            assert_eq!((n.unwrap(), back.len()), (size, size));
            placed.push((at, again));
            at += size as u64 + 3;
        }
        let mut probe = [0u8; 1];
        assert_eq!(within(file.read_at(&mut probe, at + 100)).await.unwrap(), 0);
        let err = within(file.read_exact_at(&mut probe, at + 100))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            within(seek(&mut file, SeekFrom::Current(0))).await.unwrap(),
            4,
            "a positional call moved the cursor"
        );
        within(write_all(&mut file, b"tail")).await.unwrap();
        within(flush(&mut file)).await.unwrap();
        placed
    });
    let got = std::fs::read(&path).unwrap();
    assert_eq!(&got[..8], b"headtail", "the cursor write went elsewhere");
    for (at, data) in written {
        let at = at as usize;
        assert!(
            got[at..at + data.len()] == data[..],
            "std sees other bytes at {at}"
        );
    }
}

#[test]
fn sixteen_tasks_write_disjoint_ranges_of_one_handle() {
    let rt = runtime();
    let tmp = Temp::new("pos-sixteen");
    let path = tmp.file("data");
    const EACH: usize = 65537;
    rt.block_on(async {
        let file = Arc::new(Handle::new(read_write(&path)).unwrap());
        let writers: Vec<_> = (0..16u8)
            .map(|i| {
                let file = Arc::clone(&file);
                spawn(async move {
                    let at = u64::from(i) * EACH as u64;
                    let data = seeded(EACH, i);
                    for chunk in 0..4 {
                        let part = &data[chunk * EACH / 4..(chunk + 1) * EACH / 4];
                        file.write_all_at(part, at + (chunk * EACH / 4) as u64)
                            .await
                            .unwrap();
                    }
                    let tail = &data[4 * (EACH / 4)..];
                    file.write_all_at(tail, at + (4 * (EACH / 4)) as u64)
                        .await
                        .unwrap();
                    let mut got = vec![0u8; EACH];
                    file.read_exact_at(&mut got, at).await.unwrap();
                    assert!(got == data, "task {i} read back other bytes");
                })
            })
            .collect();
        for w in writers {
            long(w).await;
        }
        let mut whole = vec![0u8; 16 * EACH];
        long(file.read_exact_at(&mut whole, 0)).await.unwrap();
        for i in 0..16u8 {
            let at = usize::from(i) * EACH;
            assert!(
                whole[at..at + EACH] == seeded(EACH, i)[..],
                "range {i} changed"
            );
        }
    });
    let got = std::fs::read(&path).unwrap();
    assert_eq!(got.len(), 16 * EACH);
    for i in 0..16u8 {
        let at = usize::from(i) * EACH;
        assert!(
            got[at..at + EACH] == seeded(EACH, i)[..],
            "std sees range {i} changed"
        );
    }
}

/// A cursor write still queued when a positional write of the same bytes
/// comes: the positional write waits for it, so it is not lost under it,
/// even when it would go to the ring while the cursor's is on the pool.
#[test]
fn a_positional_write_lands_after_the_cursor_writes_before_it() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("pos-after-cursor");
    let path = tmp.file("data");
    let (cursor, positional) = (vec![b'c'; 100_000], vec![b'p'; 4096]);
    let open = path.clone();
    rt.block_on(async {
        within(spawn(async move {
            let mut file = Handle::new(read_write(&open)).unwrap();
            file.file().unwrap().keep_on_pool();
            let (hold, busy) = hold_the_pool();
            within(write_all(&mut file, &cursor)).await.unwrap();
            file.file().unwrap().back_to_ring();
            let mut late = Box::pin(file.write_at(&positional, 1000));
            pends(&mut late).await;
            hold.send(()).unwrap();
            within(busy).await;
            assert_eq!(within(late).await.unwrap(), 4096);
            within(flush(&mut file)).await.unwrap();
        }))
        .await;
    });
    let got = std::fs::read(&path).unwrap();
    assert_eq!(got.len(), 100_000);
    assert!(
        got[1000..5096].iter().all(|&b| b == b'p'),
        "the positional write was lost under the cursor write before it"
    );
    assert!(got[..1000].iter().chain(&got[5096..]).all(|&b| b == b'c'));
}

/// Bytes read ahead before a positional write returned are not handed out
/// after it: the next cursor read sees what it wrote.
#[test]
fn a_cursor_read_after_a_positional_write_sees_it() {
    let rt = runtime();
    let tmp = Temp::new("pos-read-ahead");
    let path = tmp.file("data");
    std::fs::write(&path, vec![b'o'; 200_000]).unwrap();
    rt.block_on(async {
        let open = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut file = Handle::new(open).unwrap();
        let mut first = [0u8; 10];
        assert_eq!(within(read_some(&mut file, &mut first)).await.unwrap(), 10);
        within(file.write_all_at(b"NEW", 100)).await.unwrap();
        let mut next = [0u8; 200];
        let mut filled = 0;
        while filled < next.len() {
            filled += within(read_some(&mut file, &mut next[filled..]))
                .await
                .unwrap();
        }
        assert_eq!(
            &next[90..93],
            b"NEW",
            "a cursor read handed out bytes from before"
        );
        within(file.write_all_at(b"OWN", 300)).await.unwrap();
        let (n, got) = within(file.read_owned(Vec::with_capacity(200))).await;
        assert!(n.unwrap() >= 93, "the lent read stopped short");
        assert_eq!(
            &got[90..93],
            b"OWN",
            "a lent read handed out bytes from before"
        );
    });
}

/// A read into a task's buffer whose job read before a positional write
/// landed, and is taken after that write returned, reads again: its bytes
/// are of an older generation. On a ring the read's call goes first and is
/// served at once; on the pool either order ends the same.
#[test]
fn a_lent_read_taken_after_a_positional_write_reads_again() {
    let rt = runtime();
    let tmp = Temp::new("pos-lent-gen");
    let path = tmp.file("data");
    std::fs::write(&path, b"old!old!").unwrap();
    let got = rt.block_on(async {
        within(spawn(async move {
            let open = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let file = Handle::new(open).unwrap();
            let mut read = Box::pin(file.read_owned(Vec::with_capacity(8)));
            pends(&mut read).await;
            within(file.write_all_at(b"NEW!", 0)).await.unwrap();
            let (n, got) = within(read).await;
            assert_eq!(n.unwrap(), 8);
            got
        }))
        .await
    });
    assert_eq!(
        got, b"NEW!old!",
        "the read counted bytes from before the positional write"
    );
}

/// A positional write whose future was dropped while it waited in the
/// pool's queue still lands, before a positional write that came after.
#[test]
fn a_dropped_positional_write_lands_before_the_positional_writes_after_it() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("pos-orphan-pos");
    let path = tmp.file("data");
    std::fs::write(&path, vec![b'.'; 8192]).unwrap();
    rt.block_on(async {
        let file = Handle::new(read_write(&path)).unwrap();
        file.file().unwrap().keep_on_pool();
        let (hold, busy) = hold_the_pool();
        let first = vec![b'1'; 4096];
        let mut dropped = Box::pin(file.write_at(&first, 0));
        pends(&mut dropped).await;
        drop(dropped);
        file.file().unwrap().back_to_ring();
        let second = vec![b'2'; 4096];
        let mut after = Box::pin(file.write_at(&second, 0));
        if on_a_ring() {
            pends(&mut after).await;
        }
        hold.send(()).unwrap();
        within(busy).await;
        assert_eq!(within(after).await.unwrap(), 4096);
    });
    let got = std::fs::read(&path).unwrap();
    assert!(
        got[..4096].iter().all(|&b| b == b'2'),
        "the dropped write landed over the one after it"
    );
}

/// As above, before a write at the cursor that came after.
#[test]
fn a_dropped_positional_write_lands_before_the_cursor_writes_after_it() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("pos-orphan-cursor");
    let path = tmp.file("data");
    std::fs::write(&path, vec![b'.'; 8192]).unwrap();
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        file.file().unwrap().keep_on_pool();
        let (hold, busy) = hold_the_pool();
        let first = vec![b'1'; 4096];
        let mut dropped = Box::pin(file.write_at(&first, 0));
        pends(&mut dropped).await;
        drop(dropped);
        file.file().unwrap().back_to_ring();
        within(write_all(&mut file, &[b'3'; 4096])).await.unwrap();
        hold.send(()).unwrap();
        within(busy).await;
        within(flush(&mut file)).await.unwrap();
    });
    let got = std::fs::read(&path).unwrap();
    assert!(
        got[..4096].iter().all(|&b| b == b'3'),
        "the dropped write landed over the cursor write after it"
    );
}

/// A dropped positional write and its handle: the write lands all the same.
#[test]
fn a_dropped_positional_write_lands_after_its_handle_went() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("pos-orphan-handle");
    let path = tmp.file("data");
    let data = pattern(1 << 20);
    let expected = data.clone();
    let open = path.clone();
    rt.block_on(async move {
        let file = Handle::new(read_write(&open)).unwrap();
        file.file().unwrap().keep_on_pool();
        let (release, busy) = hold_the_pool();
        let mut out = Box::pin(file.write_at_owned(data, 0));
        pends(&mut out).await;
        drop(out);
        drop(file);
        release.send(()).unwrap();
        within(busy).await;
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(50);
    while std::fs::read(&path).unwrap() != expected {
        assert!(Instant::now() < deadline, "the write never landed");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    }
}

/// On a file opened to append, a positional write lands where the
/// system's own positional write lands (on Linux, at the end).
#[test]
fn a_positional_write_to_an_appending_file_lands_as_the_systems() {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;
    let rt = runtime();
    let tmp = Temp::new("pos-append");
    let (ours, theirs) = (tmp.file("ours"), tmp.file("theirs"));
    for p in [&ours, &theirs] {
        std::fs::write(p, b"0123456789").unwrap();
    }
    let appending = |p: &PathBuf| OpenOptions::new().append(true).open(p).unwrap();
    #[cfg(unix)]
    appending(&theirs).write_at(b"AB", 2).unwrap();
    #[cfg(windows)]
    let _ = appending(&theirs).seek_write(b"AB", 2);
    rt.block_on(async {
        let file = Handle::new(appending(&ours)).unwrap();
        let _ = within(file.write_at(b"AB", 2)).await;
    });
    assert_eq!(
        std::fs::read(&ours).unwrap(),
        std::fs::read(&theirs).unwrap()
    );
}

/// A pipe has no offset: a positional call fails as the system's does.
#[cfg(unix)]
#[test]
fn a_positional_call_on_a_pipe_fails_as_the_systems() {
    use std::os::fd::OwnedFd;
    let rt = runtime();
    let (r, _w) = std::io::pipe().unwrap();
    rt.block_on(async {
        let reader = Handle::new(File::from(OwnedFd::from(r))).unwrap();
        let mut buf = [0u8; 4];
        let err = within(reader.read_at(&mut buf, 0)).await.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ESPIPE));
    });
}
