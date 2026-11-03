//! Writing a directory's entries through to the device.

use std::io;
use std::path::Path;

use super::ops::blocking;

/// Writes the entries of the directory at `path` through to the device: the
/// names created, removed or renamed in it before the call survive a crash.
/// A file's own `sync_all` does not cover its name in the directory, so the
/// name needs this call. It runs on the blocking pool.
///
/// To replace a file `target` atomically, so a crash leaves either the old
/// contents or the new and never a mix:
///
/// 1. write the new contents to a temporary file in the same directory,
/// 2. [`sync_all`](super::File::sync_all) the temporary file,
/// 3. [`rename`](super::rename) it over `target`,
/// 4. `sync_dir` the directory they are in.
///
/// On Windows the directory is opened with backup semantics for reading and
/// writing and flushed (`FlushFileBuffers`). On Apple's systems the entries go
/// through with `F_FULLFSYNC`, which flushes the drive's cache too: APFS,
/// HFS+ and FAT take it for a directory (macOS 15 and 26). A file system
/// that refuses it gets `F_BARRIERFSYNC`, or, refusing that as well,
/// `fsync`. Neither of those flushes the drive's cache, so there the call
/// answers `Ok` once the entries reached the drive, and a power loss may
/// still lose them.
///
/// # Errors
///
/// The system's own: `path` does not exist, cannot be opened, or is not
/// something the system lets be synced. A path that is not a directory gets
/// the answer the system gives for it.
pub async fn sync_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    blocking(move || sync_dir_now(&path)).await
}

/// std's `sync_all`, which is `F_FULLFSYNC` on Apple's systems; there a file system that
/// refuses it gets the strongest call it takes after it, `F_BARRIERFSYNC` and then `fsync`
/// (`proofs/tla/dirsync/DirSync.tla`). Any other error is the answer.
#[cfg(unix)]
fn sync_dir_now(path: &Path) -> io::Result<()> {
    let dir = std::fs::File::open(path)?;
    let synced = dir.sync_all();
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    if synced.as_ref().is_err_and(refused) {
        use std::os::fd::AsRawFd;
        let fd = dir.as_raw_fd();
        // SAFETY: plain calls on the descriptor `dir` keeps open.
        let barrier = again(|| unsafe { libc::fcntl(fd, libc::F_BARRIERFSYNC) });
        if !barrier.as_ref().is_err_and(refused) {
            return barrier;
        }
        // SAFETY: as above.
        return again(|| unsafe { libc::fsync(fd) });
    }
    synced
}

/// Whether a sync's error says the file system refuses that call.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn refused(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL))
}

/// `call`'s answer, made again while a signal cuts it short.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn again(mut call: impl FnMut() -> libc::c_int) -> io::Result<()> {
    loop {
        if call() != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    }
}

#[cfg(windows)]
fn sync_dir_now(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(crate::syscore::windows::FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?
        .sync_all()
}
