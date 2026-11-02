//! The unix calls on the space a file uses and on ranges of files: reserving,
//! punching and zeroing a range, writing a range back, telling two
//! descriptors apart, and copying a range in the kernel. Each runs on a
//! descriptor the caller keeps open for the call. `proofs/tla/space/Space.tla`
//! checks FreeBSD's calls on a range against other writers of the file.

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
        target_os = "ios",
        target_os = "freebsd"
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

/// Changes the space the `len` bytes of the file `fd` from `offset` use, as
/// `mode` says. A reservation is `posix_fallocate(2)`, which grows the file
/// to the range's end; one that keeps the length is made only inside the
/// file. A hole and a zeroed range are `fspacectl(2)`'s `SPACECTL_DEALLOC`,
/// which frees the range where the file system punches holes (ZFS, tmpfs)
/// and writes zeros over it where it does not (UFS); for `ZeroRange` the
/// file then grows to the range's end, as a write of zeros would grow it.
#[cfg(target_os = "freebsd")]
pub(crate) fn allocate(fd: RawFd, offset: u64, len: u64, mode: Allocate) -> io::Result<()> {
    let end = offset.saturating_add(len);
    let (offset, len) = (file_offset(offset)?, file_offset(len)?);
    match mode {
        Allocate::Extend => reserve(fd, offset, len),
        // No call here reserves space past the end of a file and leaves its length:
        // `posix_fallocate` grows the file to the range's end, a truncate back frees the blocks
        // past the new end (UFS keeps none there), `fspacectl` only frees, and ZFS reserves no
        // range at all (its `VOP_ALLOCATE` is `EOPNOTSUPP`).
        Allocate::KeepSize if ends_past_the_file(fd, end)? => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this system cannot reserve space past the end of a file without growing it",
        )),
        Allocate::KeepSize => reserve(fd, offset, len),
        Allocate::PunchHole => deallocate(fd, offset, len),
        Allocate::ZeroRange => {
            deallocate(fd, offset, len)?;
            grow(fd, end)
        }
    }
}

/// Whether `fd` is a regular file shorter than `end`; any other file gets the
/// system's own answer to a reservation.
#[cfg(target_os = "freebsd")]
fn ends_past_the_file(fd: RawFd, end: u64) -> io::Result<bool> {
    let st = stat(fd)?;
    Ok(st.st_mode & libc::S_IFMT == libc::S_IFREG && end > st.st_size as u64)
}

/// `posix_fallocate(2)` of the `len` bytes of the file `fd` from `offset`,
/// which answers its error number itself.
#[cfg(any(
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "illumos",
    target_os = "solaris"
))]
fn reserve(fd: RawFd, offset: libc::off_t, len: libc::off_t) -> io::Result<()> {
    loop {
        // SAFETY: a plain call on the caller's descriptor.
        match unsafe { libc::posix_fallocate(fd, offset, len) } {
            0 => return Ok(()),
            libc::EINTR => {}
            err => return Err(io::Error::from_raw_os_error(err)),
        }
    }
}

#[cfg(target_os = "freebsd")]
type Fspacectl = unsafe extern "C" fn(
    libc::c_int,
    libc::c_int,
    *const libc::spacectl_range,
    libc::c_int,
    *mut libc::spacectl_range,
) -> libc::c_int;

/// `fspacectl(2)`, which FreeBSD's C library has from 14.0, looked up once: a
/// program that uses nuclei still starts on an older system, where a hole or
/// a zeroed range answers `Unsupported`.
#[cfg(target_os = "freebsd")]
fn fspacectl() -> Option<Fspacectl> {
    static FOUND: std::sync::OnceLock<Option<Fspacectl>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        // SAFETY: a lookup of a symbol by its name, which a C string holds.
        let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"fspacectl".as_ptr()) };
        // SAFETY: the C library's `fspacectl`, of this signature.
        (!found.is_null())
            .then(|| unsafe { core::mem::transmute::<*mut libc::c_void, Fspacectl>(found) })
    })
}

/// Makes the `len` bytes of the file `fd` from `offset` read as zeros
/// (`SPACECTL_DEALLOC`), up to the file's end; a call a signal stopped part
/// way goes on with what it left.
#[cfg(target_os = "freebsd")]
fn deallocate(fd: RawFd, offset: libc::off_t, len: libc::off_t) -> io::Result<()> {
    let Some(fspacectl) = fspacectl() else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this system cannot free or zero a range of a file",
        ));
    };
    let mut range = libc::spacectl_range {
        r_offset: offset,
        r_len: len,
    };
    let range = &raw mut range;
    loop {
        // SAFETY: the kernel reads the range and writes what it left of it into the same one,
        // as fspacectl(2) allows.
        retry(|| cvt_int(unsafe { fspacectl(fd, libc::SPACECTL_DEALLOC, range, 0, range) }))?;
        // SAFETY: our own range, which the call wrote.
        if unsafe { (*range).r_len } == 0 {
            return Ok(());
        }
    }
}

/// Grows the file `fd` to `end` bytes if it is shorter, with zeros: a write
/// of one zero at `end - 1`, which never cuts a write of someone else's past
/// it, as a truncate to `end` would. A descriptor that appends writes at the
/// file's end wherever a write is aimed, so the zero goes through a
/// descriptor of the file's own that does not (`O_EMPTY_PATH`).
#[cfg(target_os = "freebsd")]
fn grow(fd: RawFd, end: u64) -> io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    if super::file_size(fd)? >= end {
        return Ok(());
    }
    // SAFETY: a plain fcntl on the caller's descriptor.
    if cvt_int(unsafe { libc::fcntl(fd, libc::F_GETFL) })? & libc::O_APPEND == 0 {
        return super::pwrite_all(fd, &[0], end - 1);
    }
    let flags = libc::O_EMPTY_PATH | libc::O_WRONLY | libc::O_CLOEXEC;
    // SAFETY: opens the file `fd` names once more, from its empty path.
    let aimed = retry(|| cvt_int(unsafe { libc::openat(fd, c"".as_ptr(), flags) }))?;
    // SAFETY: a new descriptor nobody else owns.
    let aimed = unsafe { OwnedFd::from_raw_fd(aimed) };
    super::pwrite_all(aimed.as_raw_fd(), &[0], end - 1)
}

/// Reserves the `len` bytes of the file `fd` from `offset`, growing the file
/// to cover them (`posix_fallocate`); no other change of space is made here.
#[cfg(any(target_os = "dragonfly", target_os = "illumos", target_os = "solaris"))]
pub(crate) fn allocate(fd: RawFd, offset: u64, len: u64, mode: Allocate) -> io::Result<()> {
    if mode != Allocate::Extend {
        return Err(unsupported_mode());
    }
    reserve(fd, file_offset(offset)?, file_offset(len)?)
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
    target_os = "ios",
    target_os = "freebsd"
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
