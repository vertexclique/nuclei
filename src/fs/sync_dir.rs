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
/// writing and flushed (`FlushFileBuffers`); on Apple's systems a full flush
/// of a directory the system refuses is made with `fsync`.
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

#[cfg(unix)]
fn sync_dir_now(path: &Path) -> io::Result<()> {
    let dir = std::fs::File::open(path)?;
    let synced = dir.sync_all();
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    if let Err(err) = &synced
        && matches!(err.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL))
    {
        use std::os::fd::AsRawFd;
        // SAFETY: a plain call on the descriptor `dir` keeps open.
        return match unsafe { libc::fsync(dir.as_raw_fd()) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        };
    }
    synced
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
