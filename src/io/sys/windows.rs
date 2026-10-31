//! The Windows calls of the driver and of the handles: on sockets (their
//! kind, their context after an accept or a connect, shutdown) and on file
//! handles (what they are, positional reads and writes for the blocking
//! pool, the position, the size).
//!
//! Every call that takes a descriptor runs with the source's side held or
//! on a handle the caller keeps open for the call, so the descriptor is the
//! source's own throughout, as on unix.

use std::io;
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::windows::io::{BorrowedHandle, FromRawSocket, OwnedHandle, OwnedSocket};

use crate::io::raw::Raw;
use crate::io::source::Kind;
use crate::syscore::windows::{
    AF_INET, AF_INET6, DWORD, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, FILE_ACCESS_INFORMATION,
    FILE_APPEND_DATA, FILE_BEGIN, FILE_CURRENT, FILE_MODE_INFORMATION, FILE_SYNCHRONOUS_IO_ALERT,
    FILE_SYNCHRONOUS_IO_NONALERT, FILE_TYPE_DISK, FILE_WRITE_DATA, GetFileSizeEx, GetFileType,
    HANDLE, IN_ADDR, INVALID_SOCKET, IO_STATUS_BLOCK, IPPROTO_TCP, MSG_PEEK,
    NtQueryInformationFile, OVERLAPPED, ReadFile, SD_BOTH, SD_RECEIVE, SD_SEND, SIO_UDP_CONNRESET,
    SO_ACCEPTCONN, SO_MAX_MSG_SIZE, SO_TYPE, SO_UPDATE_ACCEPT_CONTEXT, SO_UPDATE_CONNECT_CONTEXT,
    SOCK_DGRAM, SOCK_RAW, SOCK_RDM, SOCK_STREAM, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
    SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR, SOL_SOCKET, STATUS_SUCCESS, SetFilePointerEx, ULONG,
    WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSAEMSGSIZE, WSAIoctl, WSASocketW, WriteFile,
    bind, getsockname, getsockopt, last_error, recv, recvfrom, send, sendto, setsockopt, wsa_error,
};

fn socket_of(raw: Raw) -> SOCKET {
    raw as SOCKET
}

fn handle_of(raw: Raw) -> HANDLE {
    raw as usize as HANDLE
}

fn check(ret: i32) -> io::Result<()> {
    if ret == SOCKET_ERROR {
        Err(wsa_error())
    } else {
        Ok(())
    }
}

/// An integer option of a socket.
fn int_option(raw: Raw, name: i32) -> io::Result<i32> {
    let mut value: i32 = 0;
    let mut len = size_of::<i32>() as i32;
    // SAFETY: the option is an int, written into our own.
    check(unsafe {
        getsockopt(
            socket_of(raw),
            SOL_SOCKET,
            name,
            core::ptr::from_mut(&mut value).cast(),
            &mut len,
        )
    })?;
    Ok(value)
}

/// What kind of socket `raw` is: one of messages (datagrams, raw or
/// reliable ones) works as UDP does, any other as a stream, as on unix.
pub(crate) fn kind_of_socket(raw: Raw) -> io::Result<Kind> {
    Ok(match int_option(raw, SO_TYPE)? {
        SOCK_DGRAM | SOCK_RAW | SOCK_RDM => Kind::Udp,
        _ if int_option(raw, SO_ACCEPTCONN)? != 0 => Kind::TcpListener,
        _ => Kind::Tcp,
    })
}

/// What kind of object `raw` is: a socket's kind; a disk file; or any
/// other handle (a pipe, a console), a byte stream.
pub(crate) fn kind_of(raw: Raw) -> io::Result<Kind> {
    match kind_of_socket(raw) {
        Err(err) if err.raw_os_error() == Some(crate::syscore::windows::WSAENOTSOCK) => {
            // SAFETY: a plain query on a handle the caller holds.
            Ok(match unsafe { GetFileType(handle_of(raw)) } {
                FILE_TYPE_DISK => Kind::File,
                _ => Kind::Bytes,
            })
        }
        other => other,
    }
}

/// Whether the handle `raw` was opened for synchronous IO: its calls block
/// the thread that makes them, and no completion of them reaches a port.
pub(crate) fn synchronous(raw: Raw) -> io::Result<bool> {
    let mode = file_information(raw, FILE_MODE_INFORMATION)?;
    Ok(mode & (FILE_SYNCHRONOUS_IO_ALERT | FILE_SYNCHRONOUS_IO_NONALERT) != 0)
}

/// Whether the file handle `raw` appends: it may add to the file's end and
/// not write over it (std's `append`).
pub(crate) fn appends(raw: Raw) -> io::Result<bool> {
    let access = file_information(raw, FILE_ACCESS_INFORMATION)?;
    Ok(access & FILE_APPEND_DATA != 0 && access & FILE_WRITE_DATA == 0)
}

/// What the handle `raw` tells of itself in `class`, one ULONG.
fn file_information(raw: Raw, class: u32) -> io::Result<ULONG> {
    let mut status = IO_STATUS_BLOCK {
        Status: 0,
        Information: 0,
    };
    let mut value: ULONG = 0;
    // SAFETY: the class answers with one ULONG, written into our own.
    let got = unsafe {
        NtQueryInformationFile(
            handle_of(raw),
            &mut status,
            core::ptr::from_mut(&mut value).cast(),
            size_of::<ULONG>() as ULONG,
            class,
        )
    };
    if got != STATUS_SUCCESS {
        return Err(io::Error::from_raw_os_error(
            crate::syscore::windows::status_error(got).unwrap_or(0),
        ));
    }
    Ok(value)
}

/// Shuts the halves `how` names of a stream socket down.
pub(crate) fn shutdown(raw: Raw, how: std::net::Shutdown) -> io::Result<()> {
    let how = match how {
        std::net::Shutdown::Read => SD_RECEIVE,
        std::net::Shutdown::Write => SD_SEND,
        std::net::Shutdown::Both => SD_BOTH,
    };
    // SAFETY: a plain call on a socket the caller holds.
    check(unsafe { crate::syscore::windows::shutdown(socket_of(raw), how) })
}

/// A `recv` (with `peek`, a look that leaves the bytes) on the stream socket
/// `raw` into `buf`, made only once a zero-byte receive said bytes, the end
/// or an error came: the socket blocks, and holding them it answers at once.
/// A reading half shut answers the end, as std's read does.
pub(crate) fn recv_ready(raw: Raw, buf: &mut [u8], peek: bool) -> io::Result<usize> {
    let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    let flags = if peek { MSG_PEEK } else { 0 };
    // SAFETY: the kernel writes at most `len` bytes into `buf`.
    let n = unsafe { recv(socket_of(raw), buf.as_mut_ptr(), len, flags) };
    if n == SOCKET_ERROR {
        let err = wsa_error();
        if err.raw_os_error() == Some(crate::syscore::windows::WSAESHUTDOWN) {
            return Ok(0);
        }
        return Err(err);
    }
    Ok(n as usize)
}

/// A `recvfrom` (with `peek`, a look that leaves the datagram) on the
/// datagram socket `raw` into `buf`, made only once a zero-byte look said a
/// datagram (or an error) came: the socket blocks, and holding one it answers
/// at once. A datagram longer than `buf` is cut to it, as the port's own
/// receive cuts one.
pub(crate) fn recv_from_ready(
    raw: Raw,
    buf: &mut [u8],
    peek: bool,
) -> io::Result<(usize, SocketAddr)> {
    let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    let flags = if peek { MSG_PEEK } else { 0 };
    let mut from = SOCKADDR_STORAGE::zeroed();
    let mut from_len = size_of::<SOCKADDR_STORAGE>() as i32;
    // SAFETY: the kernel writes at most `len` bytes into `buf` and at most
    // `from_len` into our own address storage.
    let n = unsafe {
        recvfrom(
            socket_of(raw),
            buf.as_mut_ptr(),
            len,
            flags,
            core::ptr::from_mut(&mut from).cast(),
            &mut from_len,
        )
    };
    let n = if n == SOCKET_ERROR {
        let err = wsa_error();
        if err.raw_os_error() != Some(WSAEMSGSIZE) {
            return Err(err);
        }
        buf.len()
    } else {
        n as usize
    };
    // SAFETY: the kernel wrote the sender's address and its length.
    let from = unsafe { socket_addr(core::ptr::from_ref(&from).cast(), from_len) }?;
    Ok((n, from))
}

/// A datagram handed to the socket `raw` by the calling task itself, to
/// `to` or else to its connected peer: with nothing of the socket's in the
/// kernel, the blocking socket takes it at once.
pub(crate) fn send_datagram_now(
    raw: Raw,
    bytes: &[u8],
    to: Option<&SocketAddr>,
) -> io::Result<usize> {
    let len = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    let n = match to {
        // SAFETY: the kernel reads at most `len` bytes of `bytes`.
        None => unsafe { send(socket_of(raw), bytes.as_ptr(), len, 0) },
        Some(addr) => {
            let (storage, to_len) = raw_addr(addr);
            // SAFETY: as above, and the address is our own storage.
            unsafe {
                sendto(
                    socket_of(raw),
                    bytes.as_ptr(),
                    len,
                    0,
                    core::ptr::from_ref(&storage).cast(),
                    to_len,
                )
            }
        }
    };
    if n == SOCKET_ERROR {
        return Err(wsa_error());
    }
    Ok(n as usize)
}

/// Gives a connection `listener` accepted the listener's context, which a
/// socket `AcceptEx` filled needs before most calls work on it.
pub(crate) fn adopt_accepted(listener: Raw, conn: &OwnedSocket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    let listener = socket_of(listener);
    // SAFETY: the option takes the listening socket, read from our own.
    check(unsafe {
        setsockopt(
            conn.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_UPDATE_ACCEPT_CONTEXT,
            core::ptr::from_ref(&listener).cast(),
            size_of::<SOCKET>() as i32,
        )
    })
}

/// Gives a socket `ConnectEx` connected its context, which it needs before
/// most calls work on it.
pub(crate) fn adopt_connected(raw: Raw) -> io::Result<()> {
    // SAFETY: the option takes no value.
    check(unsafe {
        setsockopt(
            socket_of(raw),
            SOL_SOCKET,
            SO_UPDATE_CONNECT_CONTEXT,
            core::ptr::null(),
            0,
        )
    })
}

/// A new overlapped TCP socket of address family `family`, not inherited.
pub(crate) fn tcp_socket(family: u16) -> io::Result<OwnedSocket> {
    crate::syscore::windows::winsock::start()?;
    // SAFETY: plain arguments; the socket is owned at once.
    let socket = unsafe {
        WSASocketW(
            i32::from(family),
            SOCK_STREAM,
            IPPROTO_TCP,
            core::ptr::null_mut(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if socket == INVALID_SOCKET {
        return Err(wsa_error());
    }
    // SAFETY: a new socket nobody else owns.
    Ok(unsafe { OwnedSocket::from_raw_socket(socket as u64) })
}

/// The address family of the socket `raw` (that of its local address).
pub(crate) fn family(raw: Raw) -> io::Result<u16> {
    let mut storage = SOCKADDR_STORAGE::zeroed();
    let mut len = size_of::<SOCKADDR_STORAGE>() as i32;
    // SAFETY: the kernel writes at most `len` bytes into our own storage.
    check(unsafe {
        getsockname(
            socket_of(raw),
            core::ptr::from_mut(&mut storage).cast(),
            &mut len,
        )
    })?;
    Ok(storage.ss_family)
}

/// Binds a socket to the unspecified address of `family` and any port, as
/// `ConnectEx` needs; a socket bound already stays as it is.
pub(crate) fn bind_any(raw: Raw, family: u16) -> io::Result<()> {
    let any = if family == AF_INET6 {
        SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0)
    } else {
        SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)
    };
    let (storage, len) = raw_addr(&any);
    // SAFETY: the address is `len` bytes of our own storage.
    match check(unsafe { bind(socket_of(raw), core::ptr::from_ref(&storage).cast(), len) }) {
        Err(err) if err.raw_os_error() == Some(crate::syscore::windows::WSAEINVAL) => Ok(()),
        other => other,
    }
}

/// The error Winsock's own check gives a datagram of `len` bytes on the
/// socket `raw` before it sends it: longer than the socket's
/// `SO_MAX_MSG_SIZE`.
pub(crate) fn datagram_too_long(raw: Raw, len: usize) -> Option<io::Error> {
    match int_option(raw, SO_MAX_MSG_SIZE) {
        Ok(max) => usize::try_from(max)
            .is_ok_and(|max| len > max)
            .then(|| io::Error::from_raw_os_error(crate::syscore::windows::WSAEMSGSIZE)),
        Err(err) => Some(err),
    }
}

/// Keeps an ICMP port-unreachable from failing the next receive of an
/// unconnected UDP socket (Windows' default), as an unconnected unix socket
/// behaves. A connected socket keeps the report, as a connected unix socket's
/// next receive fails once its peer is gone: without it a receive from a gone
/// peer would wait for good.
pub(crate) fn udp_connreset_off(raw: Raw) -> io::Result<()> {
    // SAFETY: the caller keeps the socket open for the call; the copy is never dropped.
    let socket = core::mem::ManuallyDrop::new(unsafe {
        std::net::UdpSocket::from_raw_socket(socket_of(raw) as _)
    });
    if socket.peer_addr().is_ok() {
        return Ok(());
    }
    let off: u32 = 0;
    let mut bytes: DWORD = 0;
    // SAFETY: the control code takes a BOOL in and nothing out.
    check(unsafe {
        WSAIoctl(
            socket_of(raw),
            SIO_UDP_CONNRESET,
            core::ptr::from_ref(&off).cast(),
            size_of::<u32>() as DWORD,
            core::ptr::null_mut(),
            0,
            &mut bytes,
            core::ptr::null_mut(),
            core::ptr::null(),
        )
    })
}

/// Binds the socket `raw` to `addr`.
pub(crate) fn bind_inet(raw: Raw, addr: &SocketAddr) -> io::Result<()> {
    let (storage, len) = raw_addr(addr);
    // SAFETY: the address is `len` bytes of our own storage.
    check(unsafe { bind(socket_of(raw), core::ptr::from_ref(&storage).cast(), len) })
}

/// Makes the socket `raw` listen, with room for `backlog` connections not
/// accepted yet.
pub(crate) fn listen(raw: Raw, backlog: u32) -> io::Result<()> {
    let backlog = i32::try_from(backlog).unwrap_or(i32::MAX);
    // SAFETY: a plain call on a socket the caller holds.
    check(unsafe { crate::syscore::windows::listen(socket_of(raw), backlog) })
}

/// A socket address in its raw form, and its length.
pub(crate) fn raw_addr(addr: &SocketAddr) -> (SOCKADDR_STORAGE, i32) {
    let mut storage = SOCKADDR_STORAGE::zeroed();
    let len = match addr {
        SocketAddr::V4(v4) => {
            let raw = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: v4.port().to_be(),
                sin_addr: IN_ADDR {
                    s_addr: v4.ip().octets(),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: the storage is larger than and aligned for the address.
            unsafe { core::ptr::write(core::ptr::from_mut(&mut storage).cast(), raw) };
            size_of::<SOCKADDR_IN>()
        }
        SocketAddr::V6(v6) => {
            let raw = SOCKADDR_IN6 {
                sin6_family: AF_INET6,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: v6.ip().octets(),
                sin6_scope_id: v6.scope_id(),
            };
            // SAFETY: as above.
            unsafe { core::ptr::write(core::ptr::from_mut(&mut storage).cast(), raw) };
            size_of::<SOCKADDR_IN6>()
        }
    };
    (storage, len as i32)
}

/// The internet address at `addr` (`len` bytes), as the kernel wrote it.
///
/// # Safety
///
/// `addr` points at `len` readable bytes of a socket address.
pub(crate) unsafe fn socket_addr(addr: *const SOCKADDR, len: i32) -> io::Result<SocketAddr> {
    let len = usize::try_from(len).unwrap_or(0);
    let family = if len >= 2 {
        // SAFETY: the caller's; the family is the first field of every
        // address.
        unsafe { (*addr).sa_family }
    } else {
        0
    };
    match family {
        AF_INET if len >= size_of::<SOCKADDR_IN>() => {
            // SAFETY: an AF_INET address of its full size.
            let raw = unsafe { core::ptr::read_unaligned(addr.cast::<SOCKADDR_IN>()) };
            Ok(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(raw.sin_addr.s_addr),
                u16::from_be(raw.sin_port),
            )))
        }
        AF_INET6 if len >= size_of::<SOCKADDR_IN6>() => {
            // SAFETY: an AF_INET6 address of its full size.
            let raw = unsafe { core::ptr::read_unaligned(addr.cast::<SOCKADDR_IN6>()) };
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(raw.sin6_addr),
                u16::from_be(raw.sin6_port),
                raw.sin6_flowinfo,
                raw.sin6_scope_id,
            )))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the kernel gave an address of a family nuclei does not read",
        )),
    }
}

/// An `OVERLAPPED` asking for a synchronous read or write at `offset`.
fn at(offset: u64) -> OVERLAPPED {
    let mut ov = OVERLAPPED::zeroed();
    ov.Offset = offset as u32;
    ov.OffsetHigh = (offset >> 32) as u32;
    ov
}

/// Reads up to `len` bytes at `offset` of the file into `buf`; zero at the
/// end of the file.
fn read_at(raw: Raw, buf: *mut u8, len: usize, offset: u64) -> io::Result<usize> {
    let mut ov = at(offset);
    let mut read: DWORD = 0;
    let len = u32::try_from(len).unwrap_or(u32::MAX);
    // SAFETY: writes at most `len` bytes at `buf`, which the caller owns.
    if unsafe { ReadFile(handle_of(raw), buf.cast(), len, &mut read, &mut ov) } == 0 {
        let err = last_error();
        return match err.raw_os_error() {
            Some(ERROR_HANDLE_EOF | ERROR_BROKEN_PIPE) => Ok(0),
            _ => Err(err),
        };
    }
    Ok(read as usize)
}

pub(crate) fn pread(raw: Raw, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    read_at(raw, buf.as_mut_ptr(), buf.len(), offset)
}

/// As [`pread`], into memory not written yet: the call writes the bytes it
/// returns, and the caller takes that many as initialized.
pub(crate) fn pread_uninit(
    raw: Raw,
    buf: &mut [MaybeUninit<u8>],
    offset: u64,
) -> io::Result<usize> {
    read_at(raw, buf.as_mut_ptr().cast(), buf.len(), offset)
}

/// Writes all of `buf` at `offset` of the file.
pub(crate) fn pwrite_all(raw: Raw, buf: &[u8], offset: u64) -> io::Result<()> {
    write_all_at(raw, buf, Some(offset))
}

/// Writes all of `buf` at the end of a file that appends, returning the
/// file's size after: the end of the bytes written, as no other writer
/// appended meanwhile.
pub(crate) fn append_all(raw: Raw, buf: &[u8]) -> io::Result<u64> {
    write_all_at(raw, buf, None)?;
    file_size(raw)
}

/// Writes all of `buf` at `offset`, or with `None` at the file's end.
fn write_all_at(raw: Raw, mut buf: &[u8], mut offset: Option<u64>) -> io::Result<()> {
    while !buf.is_empty() {
        // Both halves of the offset all ones: the file's end.
        let mut ov = at(offset.unwrap_or(u64::MAX));
        let mut written: DWORD = 0;
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: reads at most `len` bytes from `buf`.
        if unsafe {
            WriteFile(
                handle_of(raw),
                buf.as_ptr().cast(),
                len,
                &mut written,
                &mut ov,
            )
        } == 0
        {
            return Err(last_error());
        }
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[written as usize..];
        offset = offset.map(|at| at + u64::from(written));
    }
    Ok(())
}

/// Moves the file's position to `offset`, or with `None` reads it.
pub(crate) fn seek(raw: Raw, offset: Option<u64>) -> io::Result<u64> {
    let (distance, method) = match offset {
        Some(offset) => (
            i64::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput)?,
            FILE_BEGIN,
        ),
        None => (0, FILE_CURRENT),
    };
    let mut now: i64 = 0;
    // SAFETY: the call writes the new position into our own.
    if unsafe { SetFilePointerEx(handle_of(raw), distance, &mut now, method) } == 0 {
        return Err(last_error());
    }
    Ok(now as u64)
}

/// The file's size.
pub(crate) fn file_size(raw: Raw) -> io::Result<u64> {
    let mut size: i64 = 0;
    // SAFETY: the call writes the size into our own.
    if unsafe { GetFileSizeEx(handle_of(raw), &mut size) } == 0 {
        return Err(last_error());
    }
    Ok(size as u64)
}

/// A copy of the file handle (the same open file, its position shared).
pub(crate) fn duplicate_file(raw: Raw) -> io::Result<OwnedHandle> {
    // SAFETY: the caller keeps the handle open for the call.
    unsafe { BorrowedHandle::borrow_raw(handle_of(raw).cast()) }.try_clone_to_owned()
}

/// A handle of its own on the file `raw` names, opened for overlapped IO, for `TransmitFile`:
/// it reads at the offset each call names. A copy made with `DuplicateHandle` shares the
/// file's position, and on a handle opened without overlapped IO `TransmitFile` reads from
/// that position and moves it, so a send would start where an earlier one stopped and move
/// the caller's cursor.
pub(crate) fn reopen_overlapped(raw: Raw) -> io::Result<OwnedHandle> {
    use crate::syscore::windows::{
        FILE_FLAG_OVERLAPPED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ,
        INVALID_HANDLE_VALUE, ReOpenFile,
    };
    use std::os::windows::io::FromRawHandle;
    // SAFETY: the caller keeps the handle open for the call; the new handle is owned at once.
    let handle = unsafe {
        ReOpenFile(
            handle_of(raw),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_OVERLAPPED,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle just opened, owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Whether the provider has `TransmitFile`, which sends a file range in the kernel.
pub(crate) fn transmit_file_available() -> bool {
    crate::syscore::windows::winsock::ext().is_ok_and(|ext| ext.transmit_file.is_some())
}
