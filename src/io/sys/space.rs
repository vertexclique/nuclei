//! The unix calls on the space a file uses and on ranges of files: reserving,
//! punching and zeroing a range, writing a range back, telling two
//! descriptors apart, and copying a range in the kernel. Each runs on a
//! descriptor the caller keeps open for the call.

use std::io;
use std::os::fd::RawFd;

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
