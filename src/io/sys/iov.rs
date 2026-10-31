//! The vectored calls of the owned buffers: one system call that writes from
//! or reads into many buffers in order, through the descriptors a vectored
//! buffer builds (`buf::vectored::IoVec`). Where the system has no vectored
//! call of a kind (a file on Windows, positional calls on the unix systems
//! without them), the descriptors' buffers are read or written one call
//! each, in order, stopping at the first short one.

#[cfg(any(
    windows,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))
))]
use std::io;

use crate::buf::vectored::IoVec;

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::fd::RawFd;

    use super::IoVec;
    use crate::io::sys::unix::{SEND_FLAGS, cvt, file_offset, retry};

    fn count(iov: &[IoVec]) -> libc::c_int {
        libc::c_int::try_from(iov.len()).unwrap_or(libc::c_int::MAX)
    }

    /// `writev(2)` of the descriptors' bytes, on a pipe or a terminal.
    pub(crate) fn writev(fd: RawFd, iov: &[IoVec]) -> io::Result<usize> {
        // SAFETY: the kernel reads at most each descriptor's length from it.
        retry(|| cvt(unsafe { libc::writev(fd, iov.as_ptr(), count(iov)) }))
    }

    /// `sendmsg(2)` of the descriptors' bytes on a stream socket, never
    /// raising `SIGPIPE`.
    pub(crate) fn sendv(fd: RawFd, iov: &[IoVec]) -> io::Result<usize> {
        // SAFETY: an all-zero `msghdr` is valid (no name, no control data).
        let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
        msg.msg_iov = iov.as_ptr().cast_mut();
        msg.msg_iovlen = iov.len() as _;
        // SAFETY: the kernel reads at most each descriptor's length from it.
        retry(|| cvt(unsafe { libc::sendmsg(fd, &msg, SEND_FLAGS) }))
    }

    /// `readv(2)` into the descriptors' room, in order.
    pub(crate) fn readv(fd: RawFd, iov: &mut [IoVec]) -> io::Result<usize> {
        // SAFETY: the kernel writes at most each descriptor's length to it.
        retry(|| cvt(unsafe { libc::readv(fd, iov.as_ptr(), count(iov)) }))
    }

    /// `preadv(2)` into the descriptors' room at `offset`.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    pub(crate) fn preadv(fd: RawFd, iov: &mut [IoVec], offset: u64) -> io::Result<usize> {
        let at = file_offset(offset)?;
        // SAFETY: the kernel writes at most each descriptor's length to it.
        retry(|| cvt(unsafe { libc::preadv(fd, iov.as_ptr(), count(iov), at) }))
    }

    /// `pwritev(2)` of the descriptors' bytes at `offset`.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    pub(crate) fn pwritev(fd: RawFd, iov: &[IoVec], offset: u64) -> io::Result<usize> {
        let at = file_offset(offset)?;
        // SAFETY: the kernel reads at most each descriptor's length from it.
        retry(|| cvt(unsafe { libc::pwritev(fd, iov.as_ptr(), count(iov), at) }))
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    pub(crate) fn preadv(fd: RawFd, iov: &mut [IoVec], offset: u64) -> io::Result<usize> {
        super::each_read(iov, offset, |ptr, len, at| {
            // SAFETY: the descriptor's room is `len` writable bytes.
            let room = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
            crate::io::sys::pread(fd, room, at)
        })
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    pub(crate) fn pwritev(fd: RawFd, iov: &[IoVec], offset: u64) -> io::Result<usize> {
        super::each_write(iov, offset, |bytes, at| {
            crate::io::sys::pwrite_all(fd, bytes, at)
        })
    }
}

#[cfg(unix)]
pub(crate) use self::unix::*;

#[cfg(windows)]
mod windows {
    use std::io;

    use super::IoVec;
    use crate::io::raw::Raw;
    use crate::io::sys::windows::{read_at, write_all_at};

    /// The descriptors' rooms read one `ReadFile` each at `offset` on: a file
    /// on Windows has no vectored read but for unbuffered files of whole
    /// pages (`ReadFileScatter`).
    pub(crate) fn preadv(raw: Raw, iov: &mut [IoVec], offset: u64) -> io::Result<usize> {
        super::each_read(iov, offset, |ptr, len, at| read_at(raw, ptr, len, at))
    }

    /// The descriptors' bytes written one `WriteFile` each at `offset` on.
    pub(crate) fn pwritev(raw: Raw, iov: &[IoVec], offset: u64) -> io::Result<usize> {
        super::each_write(iov, offset, |bytes, at| write_all_at(raw, bytes, Some(at)))
    }
}

#[cfg(windows)]
pub(crate) use self::windows::*;

#[cfg(any(
    windows,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))
))]
/// The address and length a descriptor names.
fn parts(iov: &IoVec) -> (*mut u8, usize) {
    #[cfg(unix)]
    return (iov.iov_base.cast(), iov.iov_len);
    #[cfg(windows)]
    return (iov.buf, iov.len as usize);
}

/// Reads into each descriptor's room at its place from `offset` on, stopping
/// at the first read that comes short; answers how many bytes came.
#[cfg(any(
    windows,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))
))]
fn each_read(
    iov: &mut [IoVec],
    offset: u64,
    mut read: impl FnMut(*mut u8, usize, u64) -> io::Result<usize>,
) -> io::Result<usize> {
    let mut done = 0;
    for room in iov.iter() {
        let (ptr, len) = parts(room);
        let n = match read(ptr, len, offset + done as u64) {
            Ok(n) => n,
            Err(err) if done == 0 => return Err(err),
            Err(_) => return Ok(done),
        };
        done += n;
        if n < len {
            break;
        }
    }
    Ok(done)
}

/// Writes each descriptor's bytes whole at its place from `offset` on;
/// answers how many bytes landed.
#[cfg(any(
    windows,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))
))]
fn each_write(
    iov: &[IoVec],
    offset: u64,
    mut write: impl FnMut(&[u8], u64) -> io::Result<()>,
) -> io::Result<usize> {
    let mut done = 0;
    for bytes in iov {
        let (ptr, len) = parts(bytes);
        // SAFETY: the descriptor names `len` bytes the caller's buffer holds.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.cast_const(), len) };
        match write(bytes, offset + done as u64) {
            Ok(()) => done += len,
            Err(err) if done == 0 => return Err(err),
            Err(_) => return Ok(done),
        }
    }
    Ok(done)
}
