//! The Linux-only calls: a system call made directly, and the receive that
//! counts what it leaves (`TCP_INQ`, a Unix stream's `SO_INQ`).

use std::io;
use std::os::fd::RawFd;

use super::interrupted;

/// See [`super::unacked`]: `SIOCOUTQ` (libc names it `TIOCOUTQ`, the same value) counts the bytes
/// sent and not yet acknowledged, and those not yet sent.
pub(crate) fn unacked(fd: RawFd) -> Option<usize> {
    let mut count: libc::c_int = 0;
    // SAFETY: `TIOCOUTQ` writes one `c_int` through the pointer, valid for the call; a bad
    // descriptor is an error the call answers.
    let ret = unsafe { libc::ioctl(fd, libc::TIOCOUTQ as _, &raw mut count) };
    if ret < 0 {
        None
    } else {
        usize::try_from(count).ok()
    }
}

/// The Linux system call `nr` with `args` (up to six), made directly where the architecture's
/// call is written out (x86-64, AArch64), else through libc's `syscall`: a count, or the error of
/// the negated errno the kernel answered. glibc's wrappers add a cancellation-point check (about
/// 40 instructions) to every call, and its `syscall` a variadic call and the `errno` round trip;
/// nothing here is cancelled by `pthread_cancel`. A call of three arguments or fewer loads only
/// their registers: the kernel reads no others for it.
///
/// # Safety
/// `args` must be what the call `nr` takes, each pointer among them valid for what the kernel
/// reads and writes through it for the length of the call.
#[inline(always)]
pub(crate) unsafe fn syscall<const N: usize>(
    nr: libc::c_long,
    args: [usize; N],
) -> io::Result<usize> {
    const { assert!(N <= 6, "a system call takes six arguments at most") };
    let arg = |i: usize| args.get(i).copied().unwrap_or(0);
    let ret: isize;
    // SAFETY: the caller vouches for the arguments; the kernel answers in the result register
    // and clobbers only the registers named.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if N <= 3 {
            core::arch::asm!(
                "syscall",
                inlateout("rax") nr as isize => ret,
                in("rdi") arg(0),
                in("rsi") arg(1),
                in("rdx") arg(2),
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        } else {
            core::arch::asm!(
                "syscall",
                inlateout("rax") nr as isize => ret,
                in("rdi") arg(0),
                in("rsi") arg(1),
                in("rdx") arg(2),
                in("r10") arg(3),
                in("r8") arg(4),
                in("r9") arg(5),
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
    }
    // SAFETY: as above.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        if N <= 3 {
            core::arch::asm!(
                "svc 0",
                in("x8") nr,
                inlateout("x0") arg(0) => ret,
                in("x1") arg(1),
                in("x2") arg(2),
                options(nostack),
            );
        } else {
            core::arch::asm!(
                "svc 0",
                in("x8") nr,
                inlateout("x0") arg(0) => ret,
                in("x1") arg(1),
                in("x2") arg(2),
                in("x3") arg(3),
                in("x4") arg(4),
                in("x5") arg(5),
                options(nostack),
            );
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        // SAFETY: as above, through libc, which answers -1 and sets `errno` for an error.
        ret = unsafe { libc::syscall(nr, arg(0), arg(1), arg(2), arg(3), arg(4), arg(5)) } as isize;
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    if (-4095..0).contains(&ret) {
        return Err(io::Error::from_raw_os_error(-ret as i32));
    }
    Ok(ret as usize)
}

/// `SO_INQ` (and its control message `SCM_INQ`) of a Unix stream socket, Linux 6.17, by the
/// generic socket numbering these architectures use; elsewhere a Unix stream asks for none.
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
const SO_INQ: libc::c_int = 84;

/// Whether a Unix stream socket may count what its reads leave here (see [`enable_inq`]).
pub(crate) const UNIX_INQ: bool = cfg!(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
));

/// Asks a stream socket to report, with every receive, how many bytes its
/// receive queue still holds: a TCP socket (`tcp`) by `TCP_INQ` (Linux 4.18),
/// a Unix stream by `SO_INQ` (Linux 6.17). False where the kernel does not
/// know the option.
pub(crate) fn enable_inq(fd: RawFd, tcp: bool) -> bool {
    let (level, name) = if tcp {
        (libc::IPPROTO_TCP, libc::TCP_INQ)
    } else {
        #[cfg(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64"
        ))]
        {
            (libc::SOL_SOCKET, SO_INQ)
        }
        #[cfg(not(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64"
        )))]
        return false;
    };
    let one: libc::c_int = 1;
    // SAFETY: a plain setsockopt with a pointer to our own int.
    let ret = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (&one as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    ret == 0
}

/// Whether the stream socket `fd` has an end queued that its receive count leaves out, by a
/// `poll(2)` that waits for nothing and takes nothing: a Unix stream's every end (the peer's
/// close, reset or shutdown of writing), a TCP socket's reset. TCP counts a FIN as a byte left.
#[cfg(ring)]
pub(crate) fn end_queued(fd: RawFd, tcp: bool) -> bool {
    let ends = if tcp {
        libc::POLLERR
    } else {
        libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR
    };
    let mut look = libc::pollfd {
        fd,
        events: libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: one `pollfd` of ours and a timeout of zero: the kernel only fills `revents`.
    unsafe { libc::poll(&mut look, 1, 0) == 1 && look.revents & ends != 0 }
}

/// `recvmsg(2)` on a stream socket whose count is on (see [`enable_inq`]):
/// reads into `buf` and answers how many bytes, and how many the receive
/// queue still holds after them (a pending end of stream counts as one on TCP,
/// as none on a Unix stream, whose hangup edge says it). The second is `None`
/// when the kernel gave no count. Inlined: a call that failed, or that a
/// signal cut short, goes on out of line.
#[inline(always)]
pub(crate) fn read_inq(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, Option<usize>)> {
    // Room for one `int` of control data, aligned for a `cmsghdr`.
    // SAFETY: `CMSG_SPACE` is arithmetic on its argument.
    const CONTROL: usize =
        unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as libc::c_uint) } as usize;
    let mut control = [0u64; CONTROL.div_ceil(size_of::<u64>())];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: an all-zero `msghdr` is valid (no name, no control data).
    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = CONTROL as _;
    // SAFETY: writes at most `buf.len()` bytes into `buf` and at most
    // `CONTROL` into `control`, both of which `msg` points at. Made as a
    // system call (see `syscall`).
    let called = unsafe { syscall(libc::SYS_recvmsg, [fd as usize, (&raw mut msg) as usize, 0]) };
    let n = match called {
        Ok(n) => n,
        Err(err) => return read_inq_failed(fd, buf, err),
    };
    // SAFETY: the kernel filled `msg`'s control data; the header is read
    // only if it fits, and its data is one `int`.
    let left = unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (!cmsg.is_null() && is_inq((*cmsg).cmsg_level, (*cmsg).cmsg_type))
            .then(|| libc::CMSG_DATA(cmsg).cast::<u32>().read_unaligned() as usize)
    };
    Ok((n, left))
}

/// [`read_inq`] whose call failed with `err`: made again if a signal cut it
/// short, else the error.
#[cold]
#[inline(never)]
fn read_inq_failed(
    fd: RawFd,
    buf: &mut [u8],
    err: io::Error,
) -> io::Result<(usize, Option<usize>)> {
    if interrupted(&err) {
        return read_inq(fd, buf);
    }
    Err(err)
}

/// Whether a control message of `level` and `kind` is a receive's count of what it left.
fn is_inq(level: libc::c_int, kind: libc::c_int) -> bool {
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    ))]
    if level == libc::SOL_SOCKET && kind == SO_INQ {
        return true;
    }
    level == libc::IPPROTO_TCP && kind == libc::TCP_CM_INQ
}
