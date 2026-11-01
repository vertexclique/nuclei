//! The non-blocking system calls a source makes on its file descriptor.
//!
//! Each call takes the raw descriptor of a source whose guard the caller
//! holds (or that the caller owns outright): the source's retirement waits
//! for that guard, so the descriptor is open and is the handle's own for the
//! length of the call (see `source`). A call a signal cuts short is made
//! again. The receives that hand back an address go through std's own socket
//! types, borrowed for the call and never closed, so address conversion is
//! std's; accepts and connects build their addresses here, to make each a
//! single system call.

use std::io;
use std::mem::ManuallyDrop;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, TcpStream, UdpSocket};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net as unix;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
mod unix_addr;
#[cfg(ring)]
pub(crate) use self::linux::end_queued;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) use self::linux::{UNIX_INQ, enable_inq, read_inq, syscall};
pub(crate) use self::unix_addr::{UnixAddr, unix_addr};

/// A std socket of type `S` over `fd` for one call, never closed by it.
///
/// # Safety
///
/// `fd` is open and stays open for the borrow, and is a socket of the kind
/// `S` expects.
unsafe fn borrow<S: FromRawFd>(fd: RawFd) -> ManuallyDrop<S> {
    // SAFETY: the caller's contract; `ManuallyDrop` keeps it from closing.
    ManuallyDrop::new(unsafe { S::from_raw_fd(fd) })
}

/// The answer of a call that may have to wait: `None` for "not now"
/// (`EAGAIN`, `EWOULDBLOCK`), or the call's result (an error counted for
/// `stats`). Always inlined: it decides every call's result on the read
/// and write paths.
#[inline(always)]
pub(crate) fn ready<T>(result: io::Result<T>) -> Option<io::Result<T>> {
    match result {
        // The code itself, not std's kind (which is decoded from it).
        Err(e) if matches!(e.raw_os_error(), Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK) => {
            None
        }
        Err(e) => Some(Err(counted(e))),
        ok => Some(ok),
    }
}

/// Counts a call's failure `e` for `stats`, and hands it back.
#[cold]
#[inline(never)]
fn counted(e: io::Error) -> io::Error {
    crate::rt::counters::io_error(&e);
    e
}

/// Makes `call` again while a signal cuts it short.
#[inline(always)]
pub(super) fn retry<T>(mut call: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match call() {
            // The code itself, not std's kind (which is decoded from it).
            Err(e) if interrupted(&e) => {}
            other => return other,
        }
    }
}

/// Whether a call was cut short by a signal.
#[inline(always)]
pub(super) fn interrupted(e: &io::Error) -> bool {
    match e.raw_os_error() {
        Some(code) => code == libc::EINTR,
        None => e.kind() == io::ErrorKind::Interrupted,
    }
}

pub(super) fn cvt(ret: isize) -> io::Result<usize> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

pub(super) fn cvt_int(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// `read(2)` of up to `len` bytes into `buf`, made as a system call on Linux
/// (see [`syscall`]).
///
/// # Safety
/// `buf` must be valid for writes of `len` bytes.
#[inline]
unsafe fn read_raw(fd: RawFd, buf: *mut u8, len: usize) -> io::Result<usize> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: as the caller's.
    return unsafe { syscall(libc::SYS_read, [fd as usize, buf as usize, len]) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: as the caller's.
    return cvt(unsafe { libc::read(fd, buf.cast(), len) });
}

/// `read(2)` (also a stream socket's receive). Inlined: a call a signal cut
/// short is made again out of line.
#[inline(always)]
pub(crate) fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: writes at most `buf.len()` bytes into `buf`.
    match unsafe { read_raw(fd, buf.as_mut_ptr(), buf.len()) } {
        Err(e) if interrupted(&e) => read_again(fd, buf),
        other => other,
    }
}

/// [`read`] once a signal cut it short.
#[cold]
#[inline(never)]
fn read_again(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: writes at most `buf.len()` bytes into `buf`.
    retry(|| unsafe { read_raw(fd, buf.as_mut_ptr(), buf.len()) })
}

/// `recv(2)` with `MSG_PEEK`: a stream's bytes, left on the socket.
pub(crate) fn peek(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: writes at most `buf.len()` bytes into `buf`.
    retry(|| cvt(unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_PEEK) }))
}

/// `write(2)`.
pub(crate) fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: reads at most `buf.len()` bytes from `buf`.
    retry(|| cvt(unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) }))
}

/// `writev(2)` of `bufs` in order (one call, the first `IOV_MAX` slices).
pub(crate) fn write_bufs(fd: RawFd, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
    if let [one] = bufs {
        return write(fd, one);
    }
    let count = bufs.len().min(IOV_MAX) as libc::c_int;
    // SAFETY: `IoSlice` is `iovec` on unix; the kernel reads at most the
    // lengths of the first `count` slices.
    retry(|| cvt(unsafe { libc::writev(fd, bufs.as_ptr().cast(), count) }))
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub(crate) const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
/// Apple's sockets get `SO_NOSIGPIPE` when nuclei takes them instead.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
pub(crate) const SEND_FLAGS: libc::c_int = 0;

/// `send(2)`, never raising `SIGPIPE` for a closed peer (the error comes
/// back as `BrokenPipe` instead).
#[inline]
pub(crate) fn send(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    retry(|| {
        // SAFETY: reads at most `buf.len()` bytes from `buf`; no address is given.
        unsafe { send_raw(fd, buf.as_ptr(), buf.len()) }
    })
}

/// `send(2)` of up to `len` bytes of `buf` with `SEND_FLAGS`, made as a
/// system call on Linux (see [`syscall`]).
///
/// # Safety
/// `buf` must be valid for reads of `len` bytes.
#[inline]
unsafe fn send_raw(fd: RawFd, buf: *const u8, len: usize) -> io::Result<usize> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: as the caller's.
    return unsafe {
        syscall(
            libc::SYS_sendto,
            [fd as usize, buf as usize, len, SEND_FLAGS as usize, 0, 0],
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: as the caller's.
    return cvt(unsafe { libc::send(fd, buf.cast(), len, SEND_FLAGS) });
}

/// The most slices one vectored call takes.
const IOV_MAX: usize = 1024;

/// `send(2)` of the bytes of `bufs` in order (one call, the first
/// `IOV_MAX` slices at most), as [`send`] with no `SIGPIPE`.
#[inline]
pub(crate) fn send_bufs(fd: RawFd, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
    if let [one] = bufs {
        return send(fd, one);
    }
    send_msg(fd, bufs)
}

/// [`send_bufs`] of more than one slice, in one `sendmsg(2)`.
#[inline(never)]
fn send_msg(fd: RawFd, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
    // SAFETY: an all-zero `msghdr` is valid (no name, no control data).
    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
    let bufs = bufs.get(..IOV_MAX).unwrap_or(bufs);
    msg.msg_iov = bufs.as_ptr().cast_mut().cast();
    msg.msg_iovlen = bufs.len() as _;
    // SAFETY: `IoSlice` is `iovec` on unix; the kernel reads at most the
    // lengths of `bufs` from them and writes nothing to them.
    retry(|| cvt(unsafe { libc::sendmsg(fd, &msg, SEND_FLAGS) }))
}

/// `readv(2)` into `bufs` in order (one call, the first `IOV_MAX` slices at
/// most).
pub(crate) fn read_bufs(fd: RawFd, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
    if let [one] = bufs {
        return read(fd, one);
    }
    let count = bufs.len().min(IOV_MAX) as libc::c_int;
    // SAFETY: `IoSliceMut` is `iovec` on unix; the kernel writes at most the
    // lengths of the first `count` slices.
    retry(|| cvt(unsafe { libc::readv(fd, bufs.as_ptr().cast(), count) }))
}

/// `shutdown(2)` of the halves `how` names.
pub(crate) fn shutdown(fd: RawFd, how: std::net::Shutdown) -> io::Result<()> {
    shutdown_raw(fd, shut_how(how))
}

/// `shutdown(2)` of the halves its own `how` names.
pub(crate) fn shutdown_raw(fd: RawFd, how: libc::c_int) -> io::Result<()> {
    // SAFETY: a plain syscall on a socket the caller holds.
    cvt_int(unsafe { libc::shutdown(fd, how) }).map(drop)
}

/// `shutdown(2)`'s name for the halves `how` names.
fn shut_how(how: std::net::Shutdown) -> libc::c_int {
    match how {
        std::net::Shutdown::Read => libc::SHUT_RD,
        std::net::Shutdown::Write => libc::SHUT_WR,
        std::net::Shutdown::Both => libc::SHUT_RDWR,
    }
}

/// The pending error of a socket (`SO_ERROR`): how a non-blocking connect
/// ended.
pub(crate) fn take_error(fd: RawFd) -> io::Result<Option<io::Error>> {
    // SAFETY: see `borrow`; any socket answers SO_ERROR.
    unsafe { borrow::<TcpStream>(fd) }.take_error()
}

/// Puts `fd` in non-blocking mode.
pub(crate) fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl calls on a descriptor the caller owns.
    unsafe {
        let flags = cvt_int(libc::fcntl(fd, libc::F_GETFL))?;
        if flags & libc::O_NONBLOCK == 0 {
            cvt_int(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        }
    }
    Ok(())
}

/// Keeps a write to a closed peer from raising `SIGPIPE` on a socket of a
/// platform whose `send` has no flag for it. Not a socket: nothing to do.
pub(crate) fn set_nosigpipe(fd: RawFd) {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let one: libc::c_int = 1;
        // SAFETY: a plain setsockopt with a pointer to our own int; a
        // descriptor that is not a socket only fails it.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&one as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let _ = fd;
}

/// One integer socket option.
fn socket_option(fd: RawFd, level: libc::c_int, name: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the kernel writes at most `len` bytes into our own int.
    cvt_int(unsafe {
        libc::getsockopt(
            fd,
            level,
            name,
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
        )
    })?;
    Ok(value)
}

/// What kind of descriptor `fd` is, from the kernel: a socket by its type,
/// family and whether it listens; a regular file or block device; or any
/// other byte stream.
pub(crate) fn kind_of(fd: RawFd) -> io::Result<crate::io::source::Kind> {
    use crate::io::source::Kind;
    // SAFETY: plain data; all zeroes is a valid value.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: the kernel fills our own `stat`.
    cvt_int(unsafe { libc::fstat(fd, &mut st) })?;
    match st.st_mode & libc::S_IFMT {
        libc::S_IFSOCK => {}
        libc::S_IFREG | libc::S_IFBLK => return Ok(Kind::File),
        _ => return Ok(Kind::Bytes),
    }
    let ty = socket_option(fd, libc::SOL_SOCKET, libc::SO_TYPE)?;
    let listening = match socket_option(fd, libc::SOL_SOCKET, libc::SO_ACCEPTCONN) {
        Ok(listening) => listening != 0,
        // Some kernels (macOS for some families) do not answer: a stream
        // socket with no peer listens (one that neither listens nor is
        // connected can do no IO either way).
        Err(e) if e.raw_os_error() == Some(libc::ENOPROTOOPT) => has_no_peer(fd),
        Err(e) => return Err(e),
    };
    // SAFETY: plain data; all zeroes is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: the kernel writes at most `len` bytes of address.
    cvt_int(unsafe {
        libc::getsockname(
            fd,
            (&mut storage as *mut libc::sockaddr_storage).cast(),
            &mut len,
        )
    })?;
    let unix = libc::c_int::from(storage.ss_family) == libc::AF_UNIX;
    Ok(match (ty, unix, listening) {
        (libc::SOCK_DGRAM, true, _) => Kind::UnixDatagram,
        (libc::SOCK_DGRAM, false, _) => Kind::Udp,
        (_, true, true) => Kind::UnixListener,
        (_, false, true) => Kind::TcpListener,
        (_, true, false) => Kind::Stream,
        _ => Kind::Tcp,
    })
}

/// Whether a socket has no connected peer.
fn has_no_peer(fd: RawFd) -> bool {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: the kernel writes at most `len` bytes of address.
    let ret = unsafe {
        libc::getpeername(
            fd,
            (&mut storage as *mut libc::sockaddr_storage).cast(),
            &mut len,
        )
    };
    ret < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTCONN)
}

/// Makes a descriptor nuclei just received its own: non-blocking where the
/// call could not say so, close-on-exec, no `SIGPIPE`.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn adopt(fd: OwnedFd) -> io::Result<OwnedFd> {
    use std::os::fd::AsRawFd;
    set_nonblocking(fd.as_raw_fd())?;
    // SAFETY: a plain fcntl on our own descriptor.
    cvt_int(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
    set_nosigpipe(fd.as_raw_fd());
    Ok(fd)
}

/// A new non-blocking, close-on-exec socket.
pub(crate) fn socket(domain: libc::c_int, ty: libc::c_int) -> io::Result<OwnedFd> {
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        // SAFETY: a plain syscall; the descriptor is owned at once.
        let fd = cvt_int(unsafe {
            libc::socket(domain, ty | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0)
        })?;
        // SAFETY: a new descriptor nobody else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        // SAFETY: a plain syscall; the descriptor is owned at once.
        let fd = cvt_int(unsafe { libc::socket(domain, ty, 0) })?;
        // SAFETY: a new descriptor nobody else owns.
        adopt(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// One `accept(2)`: the new connection, non-blocking and close-on-exec, and
/// the peer's raw address.
fn accept_raw(fd: RawFd) -> io::Result<(OwnedFd, libc::sockaddr_storage, libc::socklen_t)> {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let addr = (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr>();
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let conn = retry(|| {
        // SAFETY: the kernel writes at most `len` bytes of address into
        // `storage` and says how many.
        cvt_int(unsafe {
            libc::accept4(fd, addr, &mut len, libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
        })
    })?;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let conn = retry(|| {
        // SAFETY: as above.
        cvt_int(unsafe { libc::accept(fd, addr, &mut len) })
    })?;
    // SAFETY: a new descriptor nobody else owns.
    let conn = unsafe { OwnedFd::from_raw_fd(conn) };
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let conn = adopt(conn)?;
    Ok((conn, storage, len))
}

/// A connection accepted from a TCP listener, with its peer.
pub(crate) fn accept_inet(fd: RawFd) -> io::Result<(OwnedFd, SocketAddr)> {
    let (conn, storage, len) = accept_raw(fd)?;
    Ok((conn, inet_addr(&storage, len)?))
}

/// A connection accepted from a Unix listener, with its peer.
pub(crate) fn accept_unix(fd: RawFd) -> io::Result<(OwnedFd, unix::SocketAddr)> {
    let (conn, storage, len) = accept_raw(fd)?;
    // SAFETY: a Unix listener's peers are `AF_UNIX`, whose address fits
    // `sockaddr_un`, which `sockaddr_storage` is at least as large as.
    let un = unsafe { &*(&storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_un>() };
    Ok((conn, unix_addr(un, len)?))
}

/// An internet address from its raw form.
pub(crate) fn inet_addr(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> io::Result<SocketAddr> {
    let len = len as usize;
    match libc::c_int::from(storage.ss_family) {
        libc::AF_INET if len >= size_of::<libc::sockaddr_in>() => {
            // SAFETY: the family says the storage holds a `sockaddr_in`.
            let a =
                unsafe { &*(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            Ok(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)),
                u16::from_be(a.sin_port),
            )))
        }
        libc::AF_INET6 if len >= size_of::<libc::sockaddr_in6>() => {
            // SAFETY: the family says the storage holds a `sockaddr_in6`.
            let a = unsafe {
                &*(storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>()
            };
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(a.sin6_addr.s6_addr),
                u16::from_be(a.sin6_port),
                a.sin6_flowinfo,
                a.sin6_scope_id,
            )))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the peer's address is not an internet address",
        )),
    }
}

/// The raw form of an internet address.
pub(crate) fn raw_inet(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(v4) => {
            // SAFETY: `sockaddr_storage` is large and aligned enough for any
            // address.
            let a = unsafe {
                &mut *(&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>()
            };
            a.sin_family = libc::AF_INET as libc::sa_family_t;
            a.sin_port = v4.port().to_be();
            a.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
            size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(v6) => {
            // SAFETY: as above.
            let a = unsafe {
                &mut *(&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>()
            };
            a.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            a.sin6_port = v6.port().to_be();
            a.sin6_addr.s6_addr = v6.ip().octets();
            a.sin6_flowinfo = v6.flowinfo();
            a.sin6_scope_id = v6.scope_id();
            size_of::<libc::sockaddr_in6>()
        }
    };
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    {
        storage.ss_len = len as u8;
    }
    (storage, len as libc::socklen_t)
}

/// Binds the socket `fd` to `addr`.
pub(crate) fn bind_inet(fd: RawFd, addr: &SocketAddr) -> io::Result<()> {
    let (storage, len) = raw_inet(addr);
    // SAFETY: the address is `len` bytes of our own storage.
    cvt_int(unsafe { libc::bind(fd, (&storage as *const libc::sockaddr_storage).cast(), len) })
        .map(drop)
}

/// Binds the socket `fd` to the Unix socket address `addr`.
pub(crate) fn bind_unix(fd: RawFd, addr: &UnixAddr) -> io::Result<()> {
    // SAFETY: the address is `len` bytes of the caller's `sockaddr_un`.
    cvt_int(unsafe { libc::bind(fd, addr.as_ptr(), addr.len) }).map(drop)
}

/// Makes the socket `fd` listen, with room for `backlog` connections not
/// accepted yet.
pub(crate) fn listen(fd: RawFd, backlog: u32) -> io::Result<()> {
    let backlog = libc::c_int::try_from(backlog).unwrap_or(libc::c_int::MAX);
    // SAFETY: a plain syscall on a socket the caller holds.
    cvt_int(unsafe { libc::listen(fd, backlog) }).map(drop)
}

/// Starts a connect on a non-blocking socket. `Ok(true)`: connected now;
/// `Ok(false)`: in progress, the socket turns writable when it ends.
pub(crate) fn connect_inet(fd: RawFd, addr: &SocketAddr) -> io::Result<bool> {
    let (storage, len) = raw_inet(addr);
    connect_raw(fd, (&storage as *const libc::sockaddr_storage).cast(), len)
}

/// Starts a connect of a non-blocking Unix socket to `addr`, as
/// [`connect_inet`]. A listener whose backlog is full answers `WouldBlock`.
pub(crate) fn connect_unix(fd: RawFd, addr: &UnixAddr) -> io::Result<bool> {
    connect_raw(fd, addr.as_ptr(), addr.len)
}

fn connect_raw(fd: RawFd, addr: *const libc::sockaddr, len: libc::socklen_t) -> io::Result<bool> {
    // SAFETY: `addr` points at `len` bytes of address the caller built.
    if unsafe { libc::connect(fd, addr, len) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // A signal does not stop a connect: it goes on, and ends as one in
        // progress does.
        Some(libc::EINPROGRESS) | Some(libc::EINTR) => Ok(false),
        _ => Err(err),
    }
}

/// `recvfrom(2)` into memory not written yet: the kernel writes the bytes it
/// returns, and the sender's raw address into `storage`, whose length it
/// answers with. With `peek` the datagram stays on the socket.
#[inline(always)]
fn recv_from_raw(
    fd: RawFd,
    buf: &mut [core::mem::MaybeUninit<u8>],
    peek: bool,
    storage: &mut libc::sockaddr_storage,
) -> io::Result<(usize, libc::socklen_t)> {
    let mut len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let flags = if peek { libc::MSG_PEEK } else { 0 };
    let n = retry(|| {
        // SAFETY: writes at most `buf.len()` bytes into `buf` and at most
        // `len` bytes into the storage; nothing is read from either.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return unsafe {
            syscall(
                libc::SYS_recvfrom,
                [
                    fd as usize,
                    buf.as_mut_ptr() as usize,
                    buf.len(),
                    flags as usize,
                    core::ptr::from_mut(storage) as usize,
                    (&raw mut len) as usize,
                ],
            )
        };
        // SAFETY: as above.
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        return cvt(unsafe {
            libc::recvfrom(
                fd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                flags,
                core::ptr::from_mut(storage).cast(),
                &mut len,
            )
        });
    })?;
    Ok((n, len))
}

/// `sendto(2)` of `buf` to the `len` bytes of address at `addr`, with `SEND_FLAGS`; made as a
/// system call on Linux (see [`syscall`]).
///
/// # Safety
/// `addr` must be valid for reads of `len` bytes.
unsafe fn send_to_raw(
    fd: RawFd,
    buf: &[u8],
    addr: *const libc::sockaddr,
    len: libc::socklen_t,
) -> io::Result<usize> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: reads at most `buf.len()` bytes of `buf`, and the caller's address.
    return unsafe {
        syscall(
            libc::SYS_sendto,
            [
                fd as usize,
                buf.as_ptr() as usize,
                buf.len(),
                SEND_FLAGS as usize,
                addr as usize,
                len as usize,
            ],
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: as above.
    return cvt(unsafe { libc::sendto(fd, buf.as_ptr().cast(), buf.len(), SEND_FLAGS, addr, len) });
}

/// A datagram from a UDP socket into memory not written yet, with its
/// sender; with `peek` it stays on the socket.
#[inline(always)]
pub(crate) fn recv_from_udp_uninit(
    fd: RawFd,
    buf: &mut [core::mem::MaybeUninit<u8>],
    peek: bool,
) -> io::Result<(usize, SocketAddr)> {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let (n, len) = recv_from_raw(fd, buf, peek, &mut storage)?;
    if len == 0 || libc::c_int::from(storage.ss_family) == libc::AF_UNSPEC {
        return no_sender(fd, n);
    }
    Ok((n, inet_addr(&storage, len)?))
}

/// A datagram from a UDP socket into `buf`, with its sender; with `peek` it
/// stays on the socket.
#[inline(always)]
pub(crate) fn recv_from_udp(
    fd: RawFd,
    buf: &mut [u8],
    peek: bool,
) -> io::Result<(usize, SocketAddr)> {
    // SAFETY: the kernel only writes bytes into `buf`, so it stays initialized.
    let into = unsafe { &mut *(core::ptr::from_mut(buf) as *mut [core::mem::MaybeUninit<u8>]) };
    recv_from_udp_uninit(fd, into, peek)
}

/// The sender of a datagram the kernel named none for. macOS names none for
/// a receive into an empty buffer: a connected socket takes datagrams from
/// its peer alone, whom the kernel names; one connected to nothing has no
/// sender to give, as std's receive has none.
#[cold]
#[inline(never)]
fn no_sender(fd: RawFd, n: usize) -> io::Result<(usize, SocketAddr)> {
    // SAFETY: see `borrow`; the fd is a UDP socket.
    let socket = unsafe { borrow::<UdpSocket>(fd) };
    match socket.peer_addr() {
        Ok(peer) => Ok((n, peer)),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the system named no sender for a datagram received into an empty buffer",
        )),
    }
}

/// A datagram from a Unix datagram socket into memory not written yet, with
/// its sender; with `peek` it stays on the socket.
pub(crate) fn recv_from_unix_uninit(
    fd: RawFd,
    buf: &mut [core::mem::MaybeUninit<u8>],
    peek: bool,
) -> io::Result<(usize, unix::SocketAddr)> {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { core::mem::zeroed() };
    let (n, len) = recv_from_raw(fd, buf, peek, &mut storage)?;
    // SAFETY: an `AF_UNIX` address fits `sockaddr_un`, which the storage is
    // at least as large as.
    let un = unsafe { &*(&raw const storage).cast::<libc::sockaddr_un>() };
    Ok((n, unix_addr(un, len)?))
}

/// A datagram from a Unix datagram socket into `buf`, with its sender; with
/// `peek` it stays on the socket. A receive is std's own, whose address the
/// kernel writes where std keeps it, so nothing is copied or looked through
/// to name the sender.
#[inline(always)]
pub(crate) fn recv_from_unix(
    fd: RawFd,
    buf: &mut [u8],
    peek: bool,
) -> io::Result<(usize, unix::SocketAddr)> {
    if peek {
        // SAFETY: the kernel only writes bytes into `buf`, so it stays initialized.
        let into = unsafe { &mut *(core::ptr::from_mut(buf) as *mut [core::mem::MaybeUninit<u8>]) };
        return recv_from_unix_uninit(fd, into, true);
    }
    // SAFETY: see `borrow`; the fd is a Unix datagram socket.
    let socket = unsafe { borrow::<unix::UnixDatagram>(fd) };
    retry(|| socket.recv_from(buf))
}

/// A datagram to `addr` on a UDP socket.
pub(crate) fn send_to_udp(fd: RawFd, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
    let (raw, len) = raw_inet(&addr);
    // SAFETY: the address is `len` bytes of `raw`.
    retry(|| unsafe { send_to_raw(fd, buf, (&raw const raw).cast(), len) })
}

/// A datagram to `addr` on a Unix datagram socket.
pub(crate) fn send_to_unix(fd: RawFd, buf: &[u8], addr: &UnixAddr) -> io::Result<usize> {
    // SAFETY: the address is `len` bytes of the caller's `sockaddr_un`.
    retry(|| unsafe { send_to_raw(fd, buf, addr.as_ptr(), addr.len) })
}

/// `offset` as the system's file offset.
pub(super) fn file_offset(offset: u64) -> io::Result<libc::off_t> {
    libc::off_t::try_from(offset).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the offset is past what the file can hold",
        )
    })
}

/// `pread(2)`: reads at `offset` without moving the file's own offset.
pub(crate) fn pread(fd: RawFd, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(test)]
    file_calls::note(fd, (b'r', buf.len(), offset));
    let offset = file_offset(offset)?;
    // SAFETY: writes at most `buf.len()` bytes into `buf`.
    retry(|| cvt(unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len(), offset) }))
}

/// `pwrite(2)` until every byte is written at `offset`, without moving the
/// file's own offset.
pub(crate) fn pwrite_all(fd: RawFd, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        #[cfg(test)]
        file_calls::note(fd, (b'w', buf.len(), offset));
        let at = file_offset(offset)?;
        // SAFETY: reads at most `buf.len()` bytes from `buf`.
        let n = retry(|| cvt(unsafe { libc::pwrite(fd, buf.as_ptr().cast(), buf.len(), at) }))?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// `write(2)` until every byte is written, on a file opened to append: each
/// call lands at the file's end. Returns the file's own offset after, the
/// end of the bytes written.
pub(crate) fn append_all(fd: RawFd, mut buf: &[u8]) -> io::Result<u64> {
    while !buf.is_empty() {
        // SAFETY: reads at most `buf.len()` bytes from `buf`.
        let n = retry(|| cvt(unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) }))?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
    }
    seek(fd, None)
}

/// How a file's descriptor was opened, as its handle's IO needs to know.
pub(crate) struct FileMode {
    /// To append (`O_APPEND`): every write lands at the end.
    pub(crate) append: bool,
    /// For direct IO (`O_DIRECT`, on Linux): the kernel moves the bytes
    /// straight between the caller's memory and the device, by its alignment
    /// rules. Apple's `F_NOCACHE` sets no such rule and is not this.
    pub(crate) direct: bool,
}

/// How the file `fd` was opened (`F_GETFL`).
pub(crate) fn file_mode(fd: RawFd) -> io::Result<FileMode> {
    // SAFETY: a plain fcntl on the caller's open descriptor.
    let flags = cvt_int(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let direct = flags & libc::O_DIRECT != 0;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let direct = false;
    Ok(FileMode {
        append: flags & libc::O_APPEND != 0,
        direct,
    })
}

/// Moves the file's own offset to `offset` (`SEEK_SET`); with `None`, only
/// reads it.
pub(crate) fn seek(fd: RawFd, offset: Option<u64>) -> io::Result<u64> {
    let (to, whence) = match offset {
        Some(offset) => (file_offset(offset)?, libc::SEEK_SET),
        None => (0, libc::SEEK_CUR),
    };
    // SAFETY: a plain syscall on the caller's descriptor.
    let at = unsafe { libc::lseek(fd, to, whence) };
    if at < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(at as u64)
    }
}

/// The file's size.
pub(crate) fn file_size(fd: RawFd) -> io::Result<u64> {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    // SAFETY: the kernel fills our own `stat`.
    cvt_int(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st.st_size as u64)
}

/// A copy of `fd` (the same open file under another number), close-on-exec:
/// only the accept's probe for a free descriptor makes one, and closes it at
/// once.
pub(crate) fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: a plain fcntl on the caller's open descriptor.
    let copy = cvt_int(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: a new descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// Whether the system moves a file's bytes to a socket in the kernel.
pub(crate) const SENDS_FILES: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "freebsd"
));

/// Sends up to `len` bytes of the regular file `file` from `offset` to the
/// socket `sock` in the kernel, without them entering user space. The socket
/// is not waited for: a full one is `WouldBlock`. The count may be short, and
/// is zero at the end of the file. This reads the file, so it may block on a
/// page that is not cached.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn send_file(sock: RawFd, file: RawFd, offset: u64, len: usize) -> io::Result<usize> {
    let mut at = file_offset(offset)?;
    // SAFETY: the kernel reads the file at `at` and writes the socket; `at`
    // is ours to advance.
    retry(|| cvt(unsafe { libc::sendfile(sock, file, &mut at, len) }))
}

/// See the Linux form.
#[cfg(target_os = "macos")]
pub(crate) fn send_file(sock: RawFd, file: RawFd, offset: u64, len: usize) -> io::Result<usize> {
    let at = file_offset(offset)?;
    retry(|| {
        // In: the bytes asked for. Out: the bytes sent, also when the call
        // fails because the socket is full or a signal came.
        let mut sent = libc::off_t::try_from(len).unwrap_or(libc::off_t::MAX);
        // SAFETY: the kernel reads the file at `at` and writes the socket;
        // `sent` is ours; there is no header or trailer.
        let r = unsafe { libc::sendfile(file, sock, at, &mut sent, core::ptr::null_mut(), 0) };
        match cvt_int(r) {
            Ok(_) => Ok(sent as usize),
            Err(_) if sent > 0 => Ok(sent as usize),
            Err(err) => Err(err),
        }
    })
}

/// See the Linux form.
#[cfg(target_os = "freebsd")]
pub(crate) fn send_file(sock: RawFd, file: RawFd, offset: u64, len: usize) -> io::Result<usize> {
    let at = file_offset(offset)?;
    retry(|| {
        let mut sent: libc::off_t = 0;
        // SAFETY: the kernel reads the file at `at` and writes the socket;
        // `sent` is ours; there is no header or trailer.
        let r = unsafe { libc::sendfile(file, sock, at, len, core::ptr::null_mut(), &mut sent, 0) };
        match cvt_int(r) {
            Ok(_) => Ok(sent as usize),
            Err(_) if sent > 0 => Ok(sent as usize),
            Err(err) => Err(err),
        }
    })
}

/// No call moves a file's bytes to a socket in the kernel here: the caller
/// reads and writes instead (see [`SENDS_FILES`]).
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "freebsd"
)))]
pub(crate) fn send_file(_: RawFd, _: RawFd, _: u64, _: usize) -> io::Result<usize> {
    Err(io::ErrorKind::Unsupported.into())
}

/// `Ok` when the socket `fd` may take a write now, else `WouldBlock`.
pub(crate) fn poll_writable(fd: RawFd) -> io::Result<()> {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one valid `pollfd`; a zero timeout does not wait.
    match retry(|| cvt_int(unsafe { libc::poll(&mut poll, 1, 0) }))? {
        0 => Err(io::ErrorKind::WouldBlock.into()),
        _ => Ok(()),
    }
}

/// `read(2)` into memory not written yet: the kernel writes the bytes it
/// returns, and the caller takes that many as initialized.
pub(crate) fn read_uninit(fd: RawFd, buf: &mut [core::mem::MaybeUninit<u8>]) -> io::Result<usize> {
    // SAFETY: writes at most `buf.len()` bytes into `buf`; nothing is read
    // from it.
    retry(|| unsafe { read_raw(fd, buf.as_mut_ptr().cast(), buf.len()) })
}

/// `pread(2)` into memory not written yet, as [`read_uninit`] at `offset`.
pub(crate) fn pread_uninit(
    fd: RawFd,
    buf: &mut [core::mem::MaybeUninit<u8>],
    offset: u64,
) -> io::Result<usize> {
    #[cfg(test)]
    file_calls::note(fd, (b'r', buf.len(), offset));
    let offset = file_offset(offset)?;
    // SAFETY: writes at most `buf.len()` bytes into `buf`.
    retry(|| cvt(unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len(), offset) }))
}

/// Gives a connection a listener accepted the context it needs: nothing on
/// unix, where the kernel's accept makes the socket whole.
#[cfg(ring)]
pub(crate) fn adopt_accepted(_listener: RawFd, _conn: &OwnedFd) -> io::Result<()> {
    Ok(())
}

/// Gives a socket a connect ended the context it needs: nothing on unix.
#[cfg(ring)]
pub(crate) fn adopt_connected(_fd: RawFd) -> io::Result<()> {
    Ok(())
}

/// The file calls the blocking pool made, for a test: each `pread` and
/// `pwrite` with its descriptor, length and offset.
#[cfg(test)]
pub(crate) mod file_calls {
    use std::os::fd::RawFd;
    use std::sync::Mutex;

    /// A `pread` (`b'r'`) or a `pwrite` (`b'w'`) of `len` bytes at `at`.
    pub(crate) type Call = (u8, usize, u64);

    static CALLS: Mutex<Vec<(RawFd, Call)>> = Mutex::new(Vec::new());

    pub(crate) fn note(fd: RawFd, call: Call) {
        if let Ok(mut calls) = CALLS.lock() {
            calls.push((fd, call));
        }
    }

    /// The calls made on `fd` since the last look, in order.
    #[cfg(target_os = "linux")]
    pub(crate) fn take(fd: RawFd) -> Vec<Call> {
        let Ok(mut calls) = CALLS.lock() else {
            return Vec::new();
        };
        let (mine, rest) = core::mem::take(&mut *calls)
            .into_iter()
            .partition::<Vec<_>, _>(|(of, _)| *of == fd);
        *calls = rest;
        mine.into_iter().map(|(_, call)| call).collect()
    }
}
