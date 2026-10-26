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
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{self as unix, UnixDatagram};
use std::path::Path;

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
/// `stats`).
pub(crate) fn ready<T>(result: io::Result<T>) -> Option<io::Result<T>> {
    match result {
        // The code itself, not std's kind (which is decoded from it).
        Err(e) if matches!(e.raw_os_error(), Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK) => {
            None
        }
        Err(e) => {
            crate::rt::counters::io_error(&e);
            Some(Err(e))
        }
        ok => Some(ok),
    }
}

/// Makes `call` again while a signal cuts it short.
fn retry<T>(mut call: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match call() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

fn cvt(ret: isize) -> io::Result<usize> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

fn cvt_int(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// `read(2)` (also a stream socket's receive).
pub(crate) fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: writes at most `buf.len()` bytes into `buf`.
    retry(|| cvt(unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) }))
}

/// `write(2)`.
pub(crate) fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: reads at most `buf.len()` bytes from `buf`.
    retry(|| cvt(unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) }))
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
const SEND_FLAGS: libc::c_int = 0;

/// `send(2)`, never raising `SIGPIPE` for a closed peer (the error comes
/// back as `BrokenPipe` instead).
pub(crate) fn send(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: reads at most `buf.len()` bytes from `buf`.
    retry(|| cvt(unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), SEND_FLAGS) }))
}

/// `shutdown(2)` of the writing half.
pub(crate) fn shutdown_write(fd: RawFd) -> io::Result<()> {
    // SAFETY: a plain syscall on a socket the caller holds.
    cvt_int(unsafe { libc::shutdown(fd, libc::SHUT_WR) }).map(drop)
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

/// Starts a connect on a non-blocking socket. `Ok(true)`: connected now;
/// `Ok(false)`: in progress, the socket turns writable when it ends.
pub(crate) fn connect_inet(fd: RawFd, addr: &SocketAddr) -> io::Result<bool> {
    let (storage, len) = raw_inet(addr);
    connect_raw(fd, (&storage as *const libc::sockaddr_storage).cast(), len)
}

/// Starts a connect of a non-blocking Unix socket to `path`, as
/// [`connect_inet`]. A listener whose backlog is full answers `WouldBlock`.
pub(crate) fn connect_unix(fd: RawFd, path: &Path) -> io::Result<bool> {
    let (un, len) = raw_unix(path)?;
    connect_raw(fd, (&un as *const libc::sockaddr_un).cast(), len)
}

/// The raw form of the Unix socket address `path`.
pub(crate) fn raw_unix(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: plain data; all zeroes is a valid value.
    let mut un: libc::sockaddr_un = unsafe { core::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= un.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the socket path is too long or holds a NUL byte",
        ));
    }
    un.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, &src) in un.sun_path.iter_mut().zip(bytes) {
        *dst = src as libc::c_char;
    }
    let len = core::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    Ok((un, len as libc::socklen_t))
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

/// A datagram from a UDP socket, with its sender.
pub(crate) fn recv_from_udp(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    // SAFETY: see `borrow`; the fd is a UDP socket.
    let socket = unsafe { borrow::<UdpSocket>(fd) };
    retry(|| socket.recv_from(buf))
}

/// A datagram from a Unix datagram socket, with its sender.
pub(crate) fn recv_from_unix(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, unix::SocketAddr)> {
    // SAFETY: see `borrow`; the fd is a Unix datagram socket.
    let socket = unsafe { borrow::<UnixDatagram>(fd) };
    retry(|| socket.recv_from(buf))
}

/// A datagram to `addr` on a UDP socket.
pub(crate) fn send_to_udp(fd: RawFd, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
    // SAFETY: see `borrow`; the fd is a UDP socket.
    let socket = unsafe { borrow::<UdpSocket>(fd) };
    retry(|| socket.send_to(buf, addr))
}

/// A datagram to `path` on a Unix datagram socket.
pub(crate) fn send_to_unix(fd: RawFd, buf: &[u8], path: &Path) -> io::Result<usize> {
    // SAFETY: see `borrow`; the fd is a Unix datagram socket.
    let socket = unsafe { borrow::<UnixDatagram>(fd) };
    retry(|| socket.send_to(buf, path))
}

/// A Unix socket address from its raw form: a path, an abstract name
/// (Linux), or unnamed.
pub(crate) fn unix_addr(
    storage: &libc::sockaddr_un,
    len: libc::socklen_t,
) -> io::Result<unix::SocketAddr> {
    let base = core::mem::offset_of!(libc::sockaddr_un, sun_path);
    let path_len = (len as usize)
        .saturating_sub(base)
        .min(storage.sun_path.len());
    // SAFETY: `c_char` and `u8` have one layout; the slice is within
    // `sun_path`.
    let raw: &[u8] =
        unsafe { core::slice::from_raw_parts(storage.sun_path.as_ptr().cast(), path_len) };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let [0, name @ ..] = raw
        && !name.is_empty()
    {
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt;
        return unix::SocketAddr::from_abstract_name(name);
    }
    let path = raw.split(|&b| b == 0).next().unwrap_or_default();
    if path.is_empty() {
        return unnamed();
    }
    unix::SocketAddr::from_pathname(std::ffi::OsStr::from_bytes(path))
}

/// The unnamed address: std makes one only from an unbound socket, so it is
/// made once.
fn unnamed() -> io::Result<unix::SocketAddr> {
    static UNNAMED: std::sync::OnceLock<unix::SocketAddr> = std::sync::OnceLock::new();
    if let Some(addr) = UNNAMED.get() {
        return Ok(addr.clone());
    }
    let addr = UnixDatagram::unbound()?.local_addr()?;
    Ok(UNNAMED.get_or_init(|| addr).clone())
}

/// `offset` as the system's file offset.
fn file_offset(offset: u64) -> io::Result<libc::off_t> {
    libc::off_t::try_from(offset).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the offset is past what the file can hold",
        )
    })
}

/// `pread(2)`: reads at `offset` without moving the file's own offset.
pub(crate) fn pread(fd: RawFd, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let offset = file_offset(offset)?;
    // SAFETY: writes at most `buf.len()` bytes into `buf`.
    retry(|| cvt(unsafe { libc::pread(fd, buf.as_mut_ptr().cast(), buf.len(), offset) }))
}

/// `pwrite(2)` until every byte is written at `offset`, without moving the
/// file's own offset.
pub(crate) fn pwrite_all(fd: RawFd, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
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

/// Whether the file `fd` was opened to append (`O_APPEND`).
pub(crate) fn appends(fd: RawFd) -> io::Result<bool> {
    // SAFETY: a plain fcntl on the caller's open descriptor.
    let flags = cvt_int(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    Ok(flags & libc::O_APPEND != 0)
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

/// A copy of `fd` (the same open file under another number), close-on-exec.
/// A copy of a file's descriptor, as [`duplicate`] makes it.
pub(crate) fn duplicate_file(fd: RawFd) -> io::Result<OwnedFd> {
    duplicate(fd)
}

pub(crate) fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: a plain fcntl on the caller's open descriptor.
    let copy = cvt_int(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: a new descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// `read(2)` into memory not written yet: the kernel writes the bytes it
/// returns, and the caller takes that many as initialized.
pub(crate) fn read_uninit(fd: RawFd, buf: &mut [core::mem::MaybeUninit<u8>]) -> io::Result<usize> {
    // SAFETY: writes at most `buf.len()` bytes into `buf`; nothing is read
    // from it.
    retry(|| cvt(unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) }))
}

/// `pread(2)` into memory not written yet, as [`read_uninit`] at `offset`.
pub(crate) fn pread_uninit(
    fd: RawFd,
    buf: &mut [core::mem::MaybeUninit<u8>],
    offset: u64,
) -> io::Result<usize> {
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
