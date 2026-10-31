//! The unix calls on the space a file uses and on ranges of files: reserving,
//! punching and zeroing a range, writing a range back, telling two
//! descriptors apart, and copying a range in the kernel. Each runs on a
//! descriptor the caller keeps open for the call.

use std::io;
use std::os::fd::RawFd;

#[cfg(target_os = "linux")]
use super::cvt;
use super::cvt_int;
#[cfg_attr(
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )),
    allow(unused_imports)
)]
use super::{file_offset, retry};
use crate::fs::Allocate;

/// `fallocate(2)`'s mode word for `mode`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn fallocate_mode(mode: Allocate) -> i32 {
    match mode {
        Allocate::Extend => 0,
        Allocate::KeepSize => libc::FALLOC_FL_KEEP_SIZE,
        Allocate::PunchHole => libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
        Allocate::ZeroRange => libc::FALLOC_FL_ZERO_RANGE,
    }
}

/// Changes the space the `len` bytes of the file `fd` from `offset` use, as
/// `mode` says (`fallocate(2)`).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn allocate(fd: RawFd, offset: u64, len: u64, mode: Allocate) -> io::Result<()> {
    let (offset, len) = (file_offset(offset)?, file_offset(len)?);
    // SAFETY: a plain syscall on the caller's descriptor.
    retry(|| cvt_int(unsafe { libc::fallocate(fd, fallocate_mode(mode), offset, len) })).map(drop)
}

/// Changes the space the `len` bytes of the file `fd` from `offset` use, as
/// `mode` says: a reservation past the physical end (`F_PREALLOCATE`, which
/// can only grow a file's reservation) and, for `Extend`, a growth of the
/// size to the range's end, or `F_PUNCHHOLE`.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn allocate(fd: RawFd, offset: u64, len: u64, mode: Allocate) -> io::Result<()> {
    match mode {
        Allocate::ZeroRange => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this system cannot zero a range of a file in place",
        )),
        Allocate::PunchHole => {
            let hole = libc::fpunchhole_t {
                fp_flags: 0,
                reserved: 0,
                fp_offset: file_offset(offset)?,
                fp_length: file_offset(len)?,
            };
            // SAFETY: the kernel reads our own `fpunchhole_t`.
            retry(|| cvt_int(unsafe { libc::fcntl(fd, libc::F_PUNCHHOLE, &hole) })).map(drop)
        }
        Allocate::Extend | Allocate::KeepSize => {
            let end = offset.saturating_add(len);
            let wanted = file_offset(end.saturating_sub(super::file_size(fd)?))?;
            let end = file_offset(end)?;
            let mut store = libc::fstore_t {
                fst_flags: libc::F_ALLOCATECONTIG,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: wanted,
                fst_bytesalloc: 0,
            };
            // SAFETY: the kernel reads and writes our own `fstore_t`.
            let contiguous = cvt_int(unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut store) });
            if contiguous.is_err() {
                store.fst_flags = libc::F_ALLOCATEALL;
                // SAFETY: as above.
                retry(|| cvt_int(unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut store) }))?;
            }
            if mode == Allocate::Extend && wanted > 0 {
                // SAFETY: a plain syscall on the caller's descriptor.
                retry(|| cvt_int(unsafe { libc::ftruncate(fd, end) }))?;
            }
            Ok(())
        }
    }
}

/// Reserves the `len` bytes of the file `fd` from `offset`, growing the file
/// to cover them (`posix_fallocate`); no other change of space is made here.
#[cfg(any(
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "illumos",
    target_os = "solaris"
))]
pub(crate) fn allocate(fd: RawFd, offset: u64, len: u64, mode: Allocate) -> io::Result<()> {
    if mode != Allocate::Extend {
        return Err(unsupported_mode());
    }
    let (offset, len) = (file_offset(offset)?, file_offset(len)?);
    loop {
        // SAFETY: a plain call on the caller's descriptor; it answers the error number itself.
        match unsafe { libc::posix_fallocate(fd, offset, len) } {
            0 => return Ok(()),
            libc::EINTR => {}
            err => return Err(io::Error::from_raw_os_error(err)),
        }
    }
}

/// No call here changes the space of a range.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "illumos",
    target_os = "solaris"
)))]
pub(crate) fn allocate(_: RawFd, _: u64, _: u64, _: Allocate) -> io::Result<()> {
    Err(unsupported_mode())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
fn unsupported_mode() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "this system cannot change the space of a file in this way",
    )
}

/// Writes the dirty pages of the `len` bytes of the file `fd` from `offset`
/// back and waits for them (`sync_file_range(2)`, before, writing and
/// after); a `len` of zero is to the end of the file.
#[cfg(target_os = "linux")]
pub(crate) fn sync_file_range(fd: RawFd, offset: u64, len: u64) -> io::Result<()> {
    let (offset, len) = (file_offset(offset)?, file_offset(len)?);
    let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE
        | libc::SYNC_FILE_RANGE_WRITE
        | libc::SYNC_FILE_RANGE_WAIT_AFTER;
    // SAFETY: a plain syscall on the caller's descriptor.
    retry(|| cvt_int(unsafe { libc::sync_file_range(fd, offset, len, flags) })).map(drop)
}

fn stat(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: the kernel fills our own `stat`.
    cvt_int(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

/// Whether the descriptors `a` and `b` name one file (one device, one inode).
pub(crate) fn same_file(a: RawFd, b: RawFd) -> io::Result<bool> {
    let (a, b) = (stat(a)?, stat(b)?);
    Ok(a.st_dev == b.st_dev && a.st_ino == b.st_ino)
}

/// Copies up to `len` bytes from `from` at `from_offset` to `to` at
/// `to_offset` in the kernel (`copy_file_range(2)`), neither file's own
/// offset used; the count may be short, and is zero at the end of `from`.
#[cfg(target_os = "linux")]
pub(crate) fn copy_file_range(
    from: RawFd,
    from_offset: u64,
    to: RawFd,
    to_offset: u64,
    len: usize,
) -> io::Result<usize> {
    let mut off_in = file_offset(from_offset)? as _;
    let mut off_out = file_offset(to_offset)? as _;
    // SAFETY: the kernel reads and advances our own two offsets.
    retry(|| cvt(unsafe { libc::copy_file_range(from, &mut off_in, to, &mut off_out, len, 0) }))
}

/// No call here copies a range in the kernel: the caller reads and writes.
#[cfg(not(target_os = "linux"))]
pub(crate) fn copy_file_range(_: RawFd, _: u64, _: RawFd, _: u64, _: usize) -> io::Result<usize> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Whether the system copies a range between files in the kernel.
pub(crate) const COPIES_IN_KERNEL: bool = cfg!(target_os = "linux");

/// Whether `err` says the kernel cannot copy between these two files (or at
/// all), so the bytes are copied through a buffer instead.
pub(crate) fn kernel_copy_unavailable(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Unsupported
        || err.raw_os_error().is_some_and(|code| {
            [
                libc::ENOSYS,
                libc::EXDEV,
                libc::EOPNOTSUPP,
                libc::ENOTSUP,
                libc::EPERM,
                libc::EINVAL,
                libc::EBADF,
            ]
            .contains(&code)
        })
}
