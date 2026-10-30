//! Files opened for direct IO (`O_DIRECT`): each read and write is one call
//! of exactly the caller's length and offset, from memory placed in its
//! page as the caller's, so the kernel answers as it would answer std.
//!
//! The files go to `NUCLEI_DIRECT_DIR` when it names a directory, else to
//! the system's temporary one; a filesystem that refuses `O_DIRECT` skips
//! the tests, saying so. Which filesystem it was is printed: some (btrfs,
//! tmpfs) serve a misaligned call buffered instead of refusing it, and the
//! tests then check only that the answer is std's.

use std::os::unix::fs::{FileExt, OpenOptionsExt};

use super::durable::ring_calls;
use super::*;
use crate::fs::file::FileIo;
use crate::syscore::linux::iouring::ring::sys::Opcode;

/// A directory for files opened for direct IO, removed when it drops, or
/// `None` (said) when its filesystem refuses them.
fn direct_dir(tag: &str) -> Option<Temp> {
    let base = std::env::var_os("NUCLEI_DIRECT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!(
        "nuclei-direct-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let temp = Temp(dir);
    let probe = temp.file("probe");
    match OpenOptions::new()
        .write(true)
        .create(true)
        .custom_flags(libc::O_DIRECT)
        .open(&probe)
    {
        Ok(_) => {
            eprintln!(
                "direct IO under {} ({})",
                temp.0.display(),
                fs_name(&temp.0)
            );
            Some(temp)
        }
        Err(err) => {
            eprintln!(
                "skipping: {} ({}) refuses O_DIRECT: {err}",
                temp.0.display(),
                fs_name(&temp.0)
            );
            None
        }
    }
}

/// The name of the filesystem `dir` is on, as far as these tests know them.
fn fs_name(dir: &std::path::Path) -> &'static str {
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: plain data; all zeroes is a valid value.
    let mut st: libc::statfs = unsafe { core::mem::zeroed() };
    // SAFETY: the kernel fills our own `statfs` for our own path.
    if unsafe { libc::statfs(path.as_ptr(), &mut st) } != 0 {
        return "unknown";
    }
    match st.f_type {
        0x0102_1994 => "tmpfs",
        0x9123_683e => "btrfs",
        0xef53 => "ext4",
        0x5846_5342 => "xfs",
        _ => "another filesystem",
    }
}

/// A file of `len` bytes written by std, opened again for direct IO.
fn direct_file(dir: &Temp, len: usize) -> (PathBuf, File) {
    let path = dir.file("data");
    std::fs::write(&path, pattern(len)).unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&path)
        .unwrap();
    (path, file)
}

/// `len` bytes of `buf`, their first at a page boundary plus `shift`.
fn at_page(buf: &mut [u8], shift: usize, len: usize) -> &mut [u8] {
    let start = (4096 - buf.as_ptr() as usize % 4096) % 4096 + shift;
    &mut buf[start..start + len]
}

/// The calls the kernel was asked for on `io` since the last look, on the
/// ring or the blocking pool: (`b'r'` or `b'w'`, length, offset).
fn kernel_calls(io: &FileIo) -> Vec<(u8, usize, u64)> {
    let mut calls: Vec<_> = ring_calls()
        .map(|(_, entries)| entries)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(op, len, off, _)| match op {
            o if o == Opcode::Read as u8 => Some((b'r', len as usize, off)),
            o if o == Opcode::Write as u8 => Some((b'w', len as usize, off)),
            _ => None,
        })
        .collect();
    calls.extend(crate::io::sys::file_calls::take(io.jobs_fd()));
    calls
}

#[test]
fn a_direct_read_of_4_kib_is_one_read_of_4_kib() {
    let Some(dir) = direct_dir("read") else {
        return;
    };
    let rt = runtime();
    let (_, std_file) = direct_file(&dir, 1 << 20);
    let expected = pattern(1 << 20);
    rt.block_on(async {
        within(spawn(async move {
            let mut file = Handle::new(std_file).unwrap();
            let io = std::sync::Arc::clone(file.file().unwrap());
            let _ = kernel_calls(&io);
            let mut mem = vec![0u8; 3 * 4096];
            let page = at_page(&mut mem, 0, 4096);
            within(seek(&mut file, SeekFrom::Start(8192)))
                .await
                .unwrap();
            assert_eq!(within(read_some(&mut file, page)).await.unwrap(), 4096);
            assert!(page[..] == expected[8192..12288]);
            assert_eq!(
                kernel_calls(&io),
                [(b'r', 4096, 8192)],
                "the cursor read was not exact"
            );
            within(file.read_exact_at(page, 65536)).await.unwrap();
            assert!(page[..] == expected[65536..69632]);
            assert_eq!(
                kernel_calls(&io),
                [(b'r', 4096, 65536)],
                "the positional read was not exact"
            );
        }))
        .await;
    });
}

#[test]
fn a_direct_write_of_1_mib_is_one_write() {
    let Some(dir) = direct_dir("write") else {
        return;
    };
    let rt = runtime();
    let (path, std_file) = direct_file(&dir, 4 << 20);
    let data = pattern(3 << 20);
    let expected = data.clone();
    rt.block_on(async {
        within(spawn(async move {
            let mut file = Handle::new(std_file).unwrap();
            let io = std::sync::Arc::clone(file.file().unwrap());
            let _ = kernel_calls(&io);
            let mut mem = vec![0u8; (1 << 20) + 4096];
            let page = at_page(&mut mem, 0, 1 << 20);
            page.copy_from_slice(&data[..1 << 20]);
            within(write_all(&mut file, page)).await.unwrap();
            within(flush(&mut file)).await.unwrap();
            assert_eq!(
                kernel_calls(&io),
                [(b'w', 1 << 20, 0)],
                "the cursor write was not one"
            );
            page.copy_from_slice(&data[1 << 20..2 << 20]);
            within(file.write_all_at(page, 1 << 20)).await.unwrap();
            assert_eq!(
                kernel_calls(&io),
                [(b'w', 1 << 20, 1 << 20)],
                "the positional write was not one"
            );
            page.copy_from_slice(&data[2 << 20..]);
            within(seek(&mut file, SeekFrom::Start(2 << 20)))
                .await
                .unwrap();
            within(write_all(&mut file, page)).await.unwrap();
            within(flush(&mut file)).await.unwrap();
            let _ = kernel_calls(&io);
        }))
        .await;
    });
    let got = std::fs::read(&path).unwrap();
    assert!(got[..3 << 20] == expected[..], "the file holds other bytes");
}

/// A call whose length, offset or memory is off the alignment the kernel
/// asks of direct IO gets the answer std gets: the kernel's.
#[test]
fn a_misaligned_direct_call_gets_the_answer_std_gets() {
    let Some(dir) = direct_dir("misaligned") else {
        return;
    };
    let rt = runtime();
    let (_, std_file) = direct_file(&dir, 1 << 20);
    let theirs = std_file.try_clone().unwrap();
    let cases = [
        (4095, 0, 0),
        (4096, 1, 0),
        (4096, 0, 1),
        (4096, 0, 512),
        (512, 4096, 0),
    ];
    rt.block_on(async {
        within(spawn(async move {
            let file = Handle::new(std_file).unwrap();
            let answer = |r: io::Result<usize>| r.map_err(|e| e.raw_os_error());
            for (len, at, shift) in cases {
                let mut mem = vec![0u8; len + 8192];
                let std_read = answer(theirs.read_at(at_page(&mut mem, shift, len), at));
                let ours = answer(within(file.read_at(at_page(&mut mem, shift, len), at)).await);
                eprintln!("read of {len} at {at}, memory {shift} past a page: {ours:?}");
                assert_eq!(
                    ours, std_read,
                    "a read of {len} at {at}, {shift} past a page"
                );
                let std_write = answer(theirs.write_at(at_page(&mut mem, shift, len), at));
                let ours = answer(within(file.write_at(at_page(&mut mem, shift, len), at)).await);
                eprintln!("write of {len} at {at}, memory {shift} past a page: {ours:?}");
                assert_eq!(
                    ours, std_write,
                    "a write of {len} at {at}, {shift} past a page"
                );
            }
        }))
        .await;
    });
}

/// The memory a direct call goes through lies in its page where the
/// caller's does, and holds all of the call's bytes; a buffered call's
/// starts its buffer and moves at most the pool's largest class.
#[test]
fn a_direct_call_goes_through_memory_placed_as_the_callers() {
    let waker = std::task::Waker::noop();
    for shift in [0usize, 1, 512, 4095] {
        for len in [1usize, 4096, 65537, 1 << 20] {
            let addr = 3 * 4096 + shift;
            let (mut buf, from, n) = crate::fs::file::bounce_for_test(len, addr, true, waker)
                .expect("a buffer of the pool");
            assert_eq!((from, n), (shift, len));
            assert_eq!(buf.spare()[from..].as_ptr() as usize % 4096, shift);
            let (_, from, n) = crate::fs::file::bounce_for_test(len, addr, false, waker)
                .expect("a buffer of the pool");
            assert_eq!((from, n), (0, len.min(crate::buf::LARGEST)));
        }
    }
}
