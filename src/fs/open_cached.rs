//! An open for reading that the kernel's caches answer at once, made on the
//! task's own thread with no job: `openat2` with `RESOLVE_CACHED` (Linux
//! 5.12), for which the kernel resolves the path from its lookup cache alone
//! and answers `EAGAIN` rather than read a directory from the device.
//!
//! The lookup is not all an open may wait for: the file system's own open
//! may ask a server (a network or a user-space file system) and the file
//! itself may make it wait (the other end of a FIFO, a device, a lease
//! another process holds). So what the path names is looked at first
//! through an `O_PATH` descriptor, which opens nothing: only a regular file
//! on a file system of this machine whose open reads nothing (a known one,
//! by its magic number) is opened here, non-blocking (as every handle's
//! descriptor is made anyway, `Handle::adopt`), so not even a lease holds it
//! up. No FIFO or device is opened here unless the path is swapped for one
//! between the look and the open; that descriptor is closed at once and the
//! open goes to the pool, as a plain open of the new file would. Anything else, and every open this cannot answer for
//! whatever reason, goes to the blocking pool as every open did before,
//! where it waits as a plain open does, and the pool's answer, an error
//! included, is the one the caller gets.

#![allow(unsafe_code)]

use core::sync::atomic::{AtomicBool, Ordering};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Whether the kernel takes the call: cleared for good when it has no
/// `openat2` or no `RESOLVE_CACHED`.
static CALL: AtomicBool = AtomicBool::new(true);

/// Paths up to this long go to the kernel from a buffer on the stack.
const ON_STACK: usize = 384;

/// The regular file at `path` on one of the [`LOCAL`] file systems, opened
/// for reading as `File::open` opens it (non-blocking besides), if the
/// kernel's caches answer the open at once; `None` when the open goes to the
/// blocking pool.
pub(super) fn open(path: &Path) -> Option<File> {
    if !CALL.load(Ordering::Relaxed) {
        return None;
    }
    let bytes = path.as_os_str().as_bytes();
    // A NUL inside the path is an error the pool's open reports.
    if bytes.contains(&0) {
        return None;
    }
    let mut stack = [0u8; ON_STACK];
    let heap;
    let name = if bytes.len() < ON_STACK {
        stack[..bytes.len()].copy_from_slice(bytes);
        stack.as_ptr()
    } else {
        heap = std::ffi::CString::new(bytes).ok()?;
        heap.as_ptr().cast()
    };
    // What the path names, opened as a place only (no file's open runs),
    // and closed again before the open.
    let place = openat2(name, libc::O_PATH | libc::O_CLOEXEC)?;
    regular(&place)?;
    local(&place)?;
    drop(place);
    let file = openat2(name, libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK)?;
    // The path may have been changed in between: the file opened must be a
    // regular one, and is then as good as any a plain open would have found.
    regular(&file)?;
    #[cfg(test)]
    took::note(path);
    Some(File::from(file))
}

/// `openat2` of `name` with `flags`, resolved from the kernel's caches only;
/// `None` when it cannot be, or fails at all.
fn openat2(name: *const u8, flags: i32) -> Option<OwnedFd> {
    // SAFETY: plain data; all zeroes is a valid value, the fields set below.
    let mut how: libc::open_how = unsafe { core::mem::zeroed() };
    how.flags = flags as u64;
    how.resolve = libc::RESOLVE_CACHED;
    // SAFETY: `name` is a NUL-terminated path that outlives the call, and
    // `how` is an `open_how` of the size passed.
    let opened = unsafe {
        crate::io::sys::syscall(
            libc::SYS_openat2,
            [
                libc::AT_FDCWD as usize,
                name as usize,
                (&how as *const libc::open_how) as usize,
                size_of::<libc::open_how>(),
                0,
                0,
            ],
        )
    };
    match opened {
        // SAFETY: a descriptor the call just opened, owned by nothing else.
        Ok(fd) => Some(unsafe { OwnedFd::from_raw_fd(fd as i32) }),
        Err(err) => {
            if matches!(
                err.raw_os_error(),
                Some(libc::ENOSYS | libc::EINVAL | libc::E2BIG)
            ) {
                CALL.store(false, Ordering::Relaxed);
            }
            None
        }
    }
}

/// The file systems whose open of a regular file for reading waits for no
/// device and no server, by the magic number `statfs` gives: ext2 to ext4,
/// XFS, Btrfs, F2FS, bcachefs, ZFS, tmpfs, ramfs, overlayfs (a container's
/// root), SquashFS, EROFS, FAT and exFAT.
const LOCAL: [u32; 13] = [
    0x0000_EF53,
    0x5846_5342,
    0x9123_683E,
    0xF2F5_2010,
    0xCA45_1A4E,
    0x2FC1_2FC1,
    0x0102_1994,
    0x8584_58F6,
    0x794C_7630,
    0x7371_7368,
    0xE0F5_E1E2,
    0x0000_4D44,
    0x2011_BAB0,
];

/// `Some` when `fd` lies on one of the [`LOCAL`] file systems.
fn local(fd: &OwnedFd) -> Option<()> {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut fs: libc::statfs = unsafe { core::mem::zeroed() };
    // SAFETY: the kernel fills our own `statfs` for a descriptor we own.
    let r = unsafe { libc::fstatfs(fd.as_raw_fd(), &mut fs) };
    (r == 0 && LOCAL.contains(&(fs.f_type as u32))).then_some(())
}

/// `Some` when `fd` is a regular file.
fn regular(fd: &OwnedFd) -> Option<()> {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: the kernel fills our own `stat` for a descriptor we own.
    let r = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
    (r == 0 && st.st_mode & libc::S_IFMT == libc::S_IFREG).then_some(())
}

/// The paths opened here, for the tests to tell this open from the pool's.
#[cfg(test)]
pub(super) mod took {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    pub(super) fn note(path: &Path) {
        if let Ok(mut opened) = OPENED.lock() {
            opened.push(path.to_path_buf());
        }
    }

    /// Whether `path` lies on a file system an open here takes, so a test
    /// on another one (a network home directory) knows it cannot see one.
    pub(in crate::fs) fn on_local(path: &Path) -> bool {
        std::fs::File::open(path)
            .is_ok_and(|file| super::local(&std::os::fd::OwnedFd::from(file)).is_some())
    }

    /// Whether `path` was opened here since the last look.
    pub(in crate::fs) fn took(path: &Path) -> bool {
        let Ok(mut opened) = OPENED.lock() else {
            return false;
        };
        let before = opened.len();
        opened.retain(|p| p != path);
        opened.len() != before
    }
}
