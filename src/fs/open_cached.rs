//! An open for reading that the kernel's caches answer at once, made on the
//! task's own thread with no job: `openat2` with `RESOLVE_CACHED` (Linux
//! 5.12), for which the kernel resolves the path from its lookup cache alone
//! and answers `EAGAIN` rather than read a directory from the device.
//!
//! Only a regular file is opened here, and the open is non-blocking (as
//! every handle's descriptor is made anyway, `Handle::adopt`), so it waits
//! for nothing: not for a lease another process holds. What the path names
//! is looked at first through an `O_PATH` descriptor, which opens nothing:
//! a FIFO or a device is never opened here, so no writer of a FIFO sees a
//! reader come and go. Anything but a regular file, and every open this
//! cannot answer for whatever reason, goes to the blocking pool as every
//! open did before, where it waits as a plain open does, and the pool's
//! answer, an error included, is the one the caller gets.

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

/// The regular file at `path`, opened for reading as `File::open` opens it
/// (non-blocking besides), if the kernel's caches answer the open at once;
/// `None` when the open goes to the blocking pool.
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
    regular(&openat2(name, libc::O_PATH | libc::O_CLOEXEC)?)?;
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
