//! Changing the space a range of a file uses: reserving, keeping the length,
//! punching and zeroing, and the rules of a write of the range.

use super::owned::{hold_the_pool, one_blocking_thread, pends};
use super::*;
use crate::fs::Allocate;

const MODES: [Allocate; 4] = [
    Allocate::Extend,
    Allocate::KeepSize,
    Allocate::PunchHole,
    Allocate::ZeroRange,
];

fn open_rw(path: &PathBuf) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

fn length(path: &PathBuf) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// Whether the call worked: a file system may refuse a mode, and `Unsupported`
/// is then the answer.
fn worked(mode: Allocate, result: io::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
            eprintln!("skipping: this system refuses {mode:?}");
            false
        }
        Err(err) => panic!("{mode:?} failed: {err:?}"),
    }
}

/// Whether a reservation worked: on FreeBSD a file system may reserve no
/// space (ZFS), and the call then answers what the system's own
/// `posix_fallocate` answers on a file of the same directory. Elsewhere it
/// works.
fn reserved(mode: Allocate, result: io::Result<()>, dir: &Temp) -> bool {
    #[cfg(target_os = "freebsd")]
    if let Err(err) = &result
        && err.kind() == io::ErrorKind::Unsupported
    {
        assert_eq!(err.raw_os_error(), kernel_reserves(dir).err(), "{mode:?}");
        eprintln!("skipping: this file system reserves no space ({mode:?})");
        return false;
    }
    let _ = dir;
    result.unwrap_or_else(|err| panic!("{mode:?} failed: {err:?}"));
    true
}

/// What the system's own `posix_fallocate` answers on a file of `dir`.
#[cfg(target_os = "freebsd")]
fn kernel_reserves(dir: &Temp) -> Result<(), i32> {
    use std::os::fd::AsRawFd;
    let file = read_write(&dir.file("oracle"));
    // SAFETY: a plain call on our own open file.
    match unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, 4096) } {
        0 => Ok(()),
        err => Err(err),
    }
}

/// Makes `range` zeros with the first mode that changes bytes and works.
async fn zero_with_any_mode(file: &Handle<File>, offset: u64, len: u64) -> bool {
    for mode in [Allocate::ZeroRange, Allocate::PunchHole] {
        if worked(mode, within(file.allocate(offset, len, mode)).await) {
            return true;
        }
    }
    false
}

#[test]
fn extend_grows_the_file_to_the_end_of_the_range_as_set_len_does() {
    let rt = runtime();
    let tmp = Temp::new("alloc-extend");
    let (ours, theirs) = (tmp.file("ours"), tmp.file("theirs"));
    let landed = rt.block_on(async {
        let file = Handle::new(read_write(&ours)).unwrap();
        let landed = reserved(
            Allocate::Extend,
            within(file.allocate(4096, 8192, Allocate::Extend)).await,
            &tmp,
        );
        read_write(&theirs).set_len(4096 + 8192).unwrap();
        landed
    });
    if landed {
        assert_eq!(length(&ours), length(&theirs));
        assert!(std::fs::read(&ours).unwrap().iter().all(|&b| b == 0));
    } else {
        assert_eq!(length(&ours), 0, "a refused reservation grew the file");
    }
}

#[test]
fn keep_size_reserves_the_range_and_leaves_the_length() {
    let rt = runtime();
    let tmp = Temp::new("alloc-keep");
    let path = tmp.file("data");
    let data = pattern(100);
    std::fs::write(&path, &data).unwrap();
    // FreeBSD reserves a range only inside the file (see the case below).
    let len = if cfg!(target_os = "freebsd") {
        64
    } else {
        1 << 20
    };
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        let result = within(file.allocate(0, len, Allocate::KeepSize)).await;
        reserved(Allocate::KeepSize, result, &tmp);
    });
    assert!(std::fs::read(&path).unwrap() == data);
}

/// No call of FreeBSD's reserves space past the end of a file and leaves its
/// length: such a range answers `Unsupported` and changes nothing, one that
/// ends at the end is inside the file.
#[cfg(target_os = "freebsd")]
#[test]
fn keep_size_past_the_end_is_unsupported_on_freebsd() {
    let rt = runtime();
    let tmp = Temp::new("alloc-keep-past");
    let path = tmp.file("data");
    let data = pattern(100);
    std::fs::write(&path, &data).unwrap();
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        for (offset, len) in [(0, 101), (99, 2), (100, 1), (4096, 1 << 20)] {
            let err = within(file.allocate(offset, len, Allocate::KeepSize))
                .await
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{offset} {len}");
            assert_eq!(
                err.to_string(),
                "this system cannot reserve space past the end of a file without growing it"
            );
        }
        let at_the_end = within(file.allocate(0, 100, Allocate::KeepSize)).await;
        reserved(Allocate::KeepSize, at_the_end, &tmp);
    });
    assert!(std::fs::read(&path).unwrap() == data);
}

#[test]
fn extend_inside_the_file_changes_nothing() {
    let rt = runtime();
    let tmp = Temp::new("alloc-inside");
    let path = tmp.file("data");
    let data = pattern(8192);
    std::fs::write(&path, &data).unwrap();
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        let result = within(file.allocate(100, 200, Allocate::Extend)).await;
        reserved(Allocate::Extend, result, &tmp);
    });
    assert!(std::fs::read(&path).unwrap() == data);
}

#[test]
fn punching_a_hole_zeroes_the_range_and_keeps_the_length() {
    let rt = runtime();
    let tmp = Temp::new("alloc-punch");
    let path = tmp.file("data");
    let data = pattern(5 * 4096 + 100);
    std::fs::write(&path, &data).unwrap();
    let mut expected = data.clone();
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        let hole = within(file.allocate(4096, 2 * 4096, Allocate::PunchHole)).await;
        if !worked(Allocate::PunchHole, hole) {
            return;
        }
        expected[4096..3 * 4096].fill(0);
        assert!(std::fs::read(&path).unwrap() == expected);
        #[cfg(not(target_os = "macos"))]
        {
            within(file.allocate(4 * 4096, 1 << 20, Allocate::PunchHole))
                .await
                .unwrap();
            expected[4 * 4096..].fill(0);
            assert!(std::fs::read(&path).unwrap() == expected);
        }
    });
}

#[cfg(not(target_os = "macos"))]
#[test]
fn zeroing_a_range_past_the_end_grows_the_file_as_zeros_would() {
    let rt = runtime();
    let tmp = Temp::new("alloc-zero");
    let path = tmp.file("data");
    let data = pattern(8192);
    std::fs::write(&path, &data).unwrap();
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        let zeroed = within(file.allocate(4096, 8192, Allocate::ZeroRange)).await;
        if !worked(Allocate::ZeroRange, zeroed) {
            return;
        }
        let mut expected = data[..4096].to_vec();
        expected.resize(12288, 0);
        assert!(std::fs::read(&path).unwrap() == expected);
    });
}

#[cfg(target_os = "macos")]
#[test]
fn zeroing_a_range_is_unsupported_on_macos() {
    let rt = runtime();
    let tmp = Temp::new("alloc-zero-mac");
    let path = tmp.file("data");
    std::fs::write(&path, pattern(8192)).unwrap();
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        let err = within(file.allocate(0, 4096, Allocate::ZeroRange))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            err.to_string(),
            "this system cannot zero a range of a file in place"
        );
    });
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
#[test]
fn an_empty_range_is_the_kernels_answer() {
    let rt = runtime();
    let tmp = Temp::new("alloc-empty");
    let path = tmp.file("data");
    rt.block_on(async {
        let file = Handle::new(read_write(&path)).unwrap();
        for mode in MODES {
            let err = within(file.allocate(0, 0, mode)).await.unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "{mode:?}");
        }
    });
}

#[test]
fn a_range_past_what_a_file_can_hold_is_invalid_input() {
    let rt = runtime();
    let tmp = Temp::new("alloc-overflow");
    let path = tmp.file("data");
    rt.block_on(async {
        let file = Handle::new(read_write(&path)).unwrap();
        let ranges = [
            (u64::MAX, 1),
            (i64::MAX as u64, 1),
            (1 << 62, 1 << 62),
            (1, u64::MAX),
            (u64::MAX, u64::MAX),
        ];
        for (offset, len) in ranges {
            for mode in MODES {
                let err = within(file.allocate(offset, len, mode)).await.unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{offset} {len}");
            }
        }
    });
    assert_eq!(length(&path), 0);
}

#[test]
fn a_read_only_handle_gets_the_kernels_error() {
    let rt = runtime();
    let tmp = Temp::new("alloc-read-only");
    let path = tmp.file("data");
    std::fs::write(&path, pattern(4096)).unwrap();
    rt.block_on(async {
        let file = within(Handle::<File>::open(&path)).await.unwrap();
        let err = within(file.allocate(0, 8192, Allocate::Extend))
            .await
            .unwrap_err();
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let theirs = File::open(&path).unwrap();
            // SAFETY: a plain syscall on an open descriptor.
            assert_eq!(
                unsafe { libc::fallocate(theirs.as_raw_fd(), 0, 0, 8192) },
                -1
            );
            assert_eq!(
                err.raw_os_error(),
                std::io::Error::last_os_error().raw_os_error()
            );
        }
        #[cfg(target_os = "freebsd")]
        {
            use std::os::fd::AsRawFd;
            let theirs = File::open(&path).unwrap();
            // SAFETY: a plain call on an open descriptor.
            let refused = unsafe { libc::posix_fallocate(theirs.as_raw_fd(), 0, 8192) };
            assert_eq!(err.raw_os_error(), Some(refused));
            for mode in [Allocate::PunchHole, Allocate::ZeroRange] {
                let err = within(file.allocate(0, 4096, mode)).await.unwrap_err();
                assert_eq!(err.raw_os_error(), Some(libc::EBADF), "{mode:?}");
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        assert!(err.raw_os_error().is_some() || err.kind() == io::ErrorKind::Unsupported);
    });
    assert_eq!(length(&path), 4096);
    assert!(std::fs::read(&path).unwrap() == pattern(4096));
}

#[cfg(target_os = "linux")]
#[test]
fn a_stream_gets_the_kernels_error() {
    use std::os::fd::OwnedFd;
    let rt = runtime();
    let (r, w) = std::io::pipe().unwrap();
    rt.block_on(async {
        let writer = Handle::new(File::from(OwnedFd::from(w))).unwrap();
        let err = within(writer.allocate(0, 4096, Allocate::Extend))
            .await
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ESPIPE));
        let reader = Handle::new(File::from(OwnedFd::from(r))).unwrap();
        let err = within(reader.allocate(0, 4096, Allocate::Extend))
            .await
            .unwrap_err();
        assert_eq!(
            err.raw_os_error(),
            Some(libc::EBADF),
            "not open for writing"
        );
    });
}

/// A pipe is no file of ranges: every mode gets the system's own answer, as
/// its own `posix_fallocate` and `fspacectl` give it on the same end
/// (`ESPIPE`, and `EBADF` from `fspacectl` on the end that cannot write).
#[cfg(target_os = "freebsd")]
#[test]
fn a_stream_gets_the_kernels_error_on_freebsd() {
    use std::os::fd::{AsRawFd, OwnedFd};
    let rt = runtime();
    let (r, w) = std::io::pipe().unwrap();
    let ends: Vec<(OwnedFd, [i32; 2])> = [OwnedFd::from(w), OwnedFd::from(r)]
        .into_iter()
        .map(|end| {
            let range = libc::spacectl_range {
                r_offset: 0,
                r_len: 4096,
            };
            // SAFETY: plain calls on an open descriptor, with our own range.
            let freed = unsafe {
                libc::fspacectl(
                    end.as_raw_fd(),
                    libc::SPACECTL_DEALLOC,
                    &range,
                    0,
                    core::ptr::null_mut(),
                )
            };
            assert_eq!(freed, -1);
            let freed = io::Error::last_os_error().raw_os_error().unwrap();
            // SAFETY: as above.
            let reserved = unsafe { libc::posix_fallocate(end.as_raw_fd(), 0, 4096) };
            (end, [reserved, freed])
        })
        .collect();
    assert_eq!(ends[0].1, [libc::ESPIPE, libc::ESPIPE]);
    rt.block_on(async {
        for (end, [reserved, freed]) in ends {
            let end = Handle::new(File::from(end)).unwrap();
            for mode in MODES {
                let want = match mode {
                    Allocate::Extend | Allocate::KeepSize => reserved,
                    Allocate::PunchHole | Allocate::ZeroRange => freed,
                };
                let err = within(end.allocate(0, 4096, mode)).await.unwrap_err();
                assert_eq!(err.raw_os_error(), Some(want), "{mode:?}");
            }
        }
    });
}

/// FreeBSD zeroes a range with `fspacectl` and grows the file past it with a
/// write of one zero at its last byte: a range past the end leaves a gap of
/// zeros and the bytes before it, one inside the file keeps its length, and
/// a file opened to append grows to the range's end as well, its zero never
/// appended.
#[cfg(target_os = "freebsd")]
#[test]
fn zeroing_a_range_on_freebsd_grows_the_file_only_to_its_end() {
    let rt = runtime();
    let tmp = Temp::new("alloc-zero-bsd");
    let path = tmp.file("data");
    let data = pattern(10_000);
    std::fs::write(&path, &data).unwrap();
    rt.block_on(async {
        let file = Handle::new(open_rw(&path)).unwrap();
        within(file.allocate(3000, 2000, Allocate::ZeroRange))
            .await
            .unwrap();
        let mut want = data.clone();
        want[3000..5000].fill(0);
        assert!(std::fs::read(&path).unwrap() == want, "inside the file");
        within(file.allocate(20_000, 5000, Allocate::ZeroRange))
            .await
            .unwrap();
        want.resize(25_000, 0);
        assert!(std::fs::read(&path).unwrap() == want, "past the end");
        within(file.allocate(24_000, 1000, Allocate::ZeroRange))
            .await
            .unwrap();
        assert!(std::fs::read(&path).unwrap() == want, "up to the end");
        drop(file);
        let appending = OpenOptions::new().append(true).open(&path).unwrap();
        let file = Handle::new(appending).unwrap();
        within(file.allocate(24_990, 20, Allocate::ZeroRange))
            .await
            .unwrap();
        want.resize(25_010, 0);
        assert!(
            std::fs::read(&path).unwrap() == want,
            "a file opened to append"
        );
        within(file.allocate(0, 100, Allocate::PunchHole))
            .await
            .unwrap();
        want[..100].fill(0);
        assert!(std::fs::read(&path).unwrap() == want, "a hole in it");
    });
}

#[test]
fn a_range_waits_for_the_cursor_writes_accepted_before_it() {
    let rt = runtime();
    let tmp = Temp::new("alloc-order");
    let path = tmp.file("data");
    rt.block_on(async {
        let mut file = Handle::new(read_write(&path)).unwrap();
        within(write_all(&mut file, &[b'x'; 8192])).await.unwrap();
        if !zero_with_any_mode(&file, 0, 4096).await {
            return;
        }
        within(flush(&mut file)).await.unwrap();
        let got = std::fs::read(&path).unwrap();
        assert!(
            got[..4096].iter().all(|&b| b == 0),
            "the range was written over"
        );
        assert!(got[4096..].iter().all(|&b| b == b'x'));
    });
}

#[test]
fn a_cursor_read_after_a_hole_sees_zeros_not_the_read_ahead() {
    let rt = runtime();
    let tmp = Temp::new("alloc-read-ahead");
    let path = tmp.file("data");
    std::fs::write(&path, pattern(256 * 1024)).unwrap();
    rt.block_on(async {
        let mut file = Handle::new(open_rw(&path)).unwrap();
        let mut head = [0u8; 10];
        assert!(within(read_some(&mut file, &mut head)).await.unwrap() > 0);
        if !zero_with_any_mode(&file, 0, 128 * 1024).await {
            return;
        }
        let mut next = [0xffu8; 64];
        let n = within(read_some(&mut file, &mut next)).await.unwrap();
        assert!(
            n > 0 && next[..n].iter().all(|&b| b == 0),
            "stale bytes were handed out"
        );
    });
}

#[test]
fn a_cursor_read_after_a_cut_sees_the_new_end() {
    let rt = runtime();
    let tmp = Temp::new("alloc-cut");
    let path = tmp.file("data");
    let data = pattern(256 * 1024);
    std::fs::write(&path, &data).unwrap();
    rt.block_on(async {
        let mut file = Handle::new(open_rw(&path)).unwrap();
        let mut head = [0u8; 10];
        let n = within(read_some(&mut file, &mut head)).await.unwrap();
        assert!(n > 0);
        within(file.set_len(20)).await.unwrap();
        let rest = within(read_to_end(&mut file)).await.unwrap();
        assert!(rest == data[n..20], "the read ran past the cut");
    });
}

#[test]
fn a_dropped_range_call_still_lands_and_the_next_call_waits_for_it() {
    let rt = one_blocking_thread();
    let tmp = Temp::new("alloc-dropped");
    let path = tmp.file("data");
    // A range that grows the file: a reservation, or a zeroed range on a file
    // system of FreeBSD's that reserves no space.
    #[cfg(target_os = "freebsd")]
    let mode = if kernel_reserves(&tmp).is_ok() {
        Allocate::Extend
    } else {
        Allocate::ZeroRange
    };
    #[cfg(not(target_os = "freebsd"))]
    let mode = Allocate::Extend;
    rt.block_on(async {
        let file = Handle::new(read_write(&path)).unwrap();
        file.file().unwrap().keep_on_pool();
        let (hold, busy) = hold_the_pool();
        let mut dropped = Box::pin(file.allocate(0, 1 << 20, mode));
        pends(&mut dropped).await;
        drop(dropped);
        let mut buf = [0u8; 4];
        let mut after = Box::pin(file.read_at(&mut buf, 0));
        pends(&mut after).await;
        hold.send(()).unwrap();
        within(busy).await;
        assert_eq!(within(after).await.unwrap(), 4);
    });
    assert_eq!(length(&path), 1 << 20);
}

#[cfg(ring)]
#[test]
fn a_range_call_is_an_operation_of_the_ring() {
    use crate::syscore::linux::iouring::ring::sys::Opcode;
    let rt = super::durable::one_blocking_thread_kept();
    let tmp = Temp::new("alloc-ring");
    let path = tmp.file("data");
    rt.block_on(async {
        within(spawn(async move {
            if super::durable::ring_calls().is_none() {
                eprintln!("skipping: the shard runs no ring (NUCLEI_BACKEND or no io_uring)");
                return;
            }
            let file = Handle::new(read_write(&path)).unwrap();
            let _ = super::durable::ring_calls();
            let (hold, busy) = hold_the_pool();
            within(file.allocate(4096, 8192, Allocate::KeepSize))
                .await
                .unwrap();
            within(file.allocate(0, 4096, Allocate::Extend))
                .await
                .unwrap();
            let (_, entries) = super::durable::ring_calls().unwrap();
            let ours: Vec<_> = entries
                .iter()
                .filter(|e| e.0 == Opcode::Fallocate as u8)
                .map(|e| (e.1, e.2))
                .collect();
            assert_eq!(ours, [(libc::FALLOC_FL_KEEP_SIZE as u32, 4096), (0, 0)]);
            hold.send(()).unwrap();
            within(busy).await;
        }))
        .await;
    });
}
