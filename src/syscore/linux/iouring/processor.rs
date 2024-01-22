use std::future::Future;
use std::{io, mem};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::os::unix::net::{SocketAddr as UnixSocketAddr, UnixDatagram, UnixListener, UnixStream};
use std::path::Path;
use std::{
    fs::File,
    mem::ManuallyDrop,
    os::unix::io::{AsRawFd, FromRawFd},
};

use crate::proactor::Proactor;

use rustix_uring::{opcode as OP, types::Fd};
use crate::syscore::shim_to_af_unix;
use crate::Handle;
use std::ffi::CString;
use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::prelude::RawFd;
use std::ptr::null_mut;
use libc::sockaddr_un;
use os_socketaddr::OsSocketAddr;
use pin_utils::unsafe_pinned;
use rustix::io_uring::{IoringRecvFlags, msghdr, RecvFlags, SendFlags, sockaddr, sockaddr_storage, SocketFlags};
use rustix::net::{connect_unix, SocketAddrAny, SocketAddrStorage, SocketAddrUnix};
use rustix_uring::opcode::RecvMsg;
use rustix_uring::squeue::Entry;
use rustix_uring::types::{AtFlags, Fixed, Mode, OFlags, socklen_t, Statx, StatxFlags};
use socket2::SockAddr;

macro_rules! syscall {
    ($fn:ident $args:tt) => {{
        let res = unsafe { libc::$fn $args };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

pub struct Processor;

impl Processor {
    ///////////////////////////////////
    ///// Read Write
    ///////////////////////////////////

    pub(crate) async fn processor_open_at(path: impl AsRef<Path>) -> io::Result<usize> {
        let path = CString::new(path.as_ref().as_os_str().as_bytes()).expect("invalid path");
        let path = path.as_ptr();
        let dfd = libc::AT_FDCWD;
        let mut sqe = OP::OpenAt::new(Fd(dfd.as_raw_fd()), path)
            .flags(OFlags::CLOEXEC | OFlags::RDONLY)
            .mode(Mode::from(0o666))
            .build();

        let cc = Proactor::get().inner().register_io(sqe)?;

        let x = cc.await? as _;

        Ok(x)
    }

    pub(crate) async fn processor_read_file(
        io: &RawFd,
        buf: &mut [u8],
        offset: usize,
    ) -> io::Result<usize> {
        let mut sqe = OP::Read::new(Fd(*io), buf.as_mut_ptr(), buf.len() as _)
            .offset(offset as _)
            .build();

        let cc = Proactor::get().inner().register_io(sqe)?;

        Ok(cc.await? as _)
    }

    pub(crate) async fn processor_write_file(
        io: &RawFd,
        buf: &[u8],
        offset: usize,
    ) -> io::Result<usize> {
        let mut sqe = OP::Write::new(Fd(*io), buf.as_ptr(), buf.len() as _)
            .offset(offset as _)
            .build();

        let cc = Proactor::get().inner().register_io(sqe)?;

        Ok(cc.await? as _)
    }

    pub(crate) async fn processor_close_file(io: &RawFd) -> io::Result<usize> {
        let mut sqe = OP::Close::new(Fd(*io))
            .build();

        let cc = Proactor::get().inner().register_io(sqe)?;

        Ok(cc.await? as _)
    }

    pub(crate) async fn processor_file_size(
        io: &RawFd,
        statx: *mut Statx,
    ) -> io::Result<usize> {
        static EMPTY: libc::c_char = 0;
        let flags = libc::AT_EMPTY_PATH;
        let mask = libc::STATX_SIZE;

        let mut sqe = OP::Statx::new(Fd(*io), &EMPTY, statx)
            .flags(AtFlags::EMPTY_PATH)
            .mask(StatxFlags::SIZE)
            .build();

        Proactor::get()
            .inner()
            .register_io(sqe)?
            .await?;

        unsafe { Ok((*statx).stx_size as usize) }
    }

    pub(crate) async fn processor_read_vectored(
        io: &RawFd,
        bufs: &mut [IoSliceMut<'_>],
    ) -> io::Result<usize> {
        let mut sqe = OP::Readv::new(Fd(*io), bufs as *mut _ as *mut _, bufs.len() as _)
            .offset(0_u64)
            .build();

        let cc = Proactor::get().inner().register_io(sqe)?;

        Ok(cc.await? as _)
    }

    pub(crate) async fn processor_write_vectored(
        io: &RawFd,
        bufs: &[IoSlice<'_>],
    ) -> io::Result<usize> {
        let mut sqe = OP::Writev::new(Fd(*io), bufs as *const _ as *const _, bufs.len() as _)
            .offset(0_u64)
            .build();

        let cc = Proactor::get().inner().register_io(sqe)?;

        Ok(cc.await? as _)
    }

    ///////////////////////////////////
    ///// Send, Recv, Peek
    ///// Commonality of TcpStream, UdpSocket, UnixStream, UnixDatagram
    ///////////////////////////////////

    pub(crate) async fn processor_send<R: AsRawFd>(socket: &R, buf: &[u8]) -> io::Result<usize> {
        let fd = socket.as_raw_fd() as _;

        let mut sqe = OP::Send::new(Fd(fd), buf.as_ptr() as _, buf.len() as _)
            .flags(SendFlags::empty())
            .build();

        let res = Proactor::get()
            .inner()
            .register_io(sqe)?
            .await?;

        Ok(res as _)
    }

    pub(crate) async fn processor_recv<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
        Self::recv_with_flags(sock, buf, RecvFlags::empty()).await
    }

    pub(crate) async fn processor_peek<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
        Self::recv_with_flags(sock, buf, RecvFlags::PEEK).await
    }

    async fn recv_with_flags<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
        flags: RecvFlags,
    ) -> io::Result<usize> {
        let fd = socket.as_raw_fd() as _;

        let mut sqe = OP::Recv::new(Fd(fd), buf.as_mut_ptr(), buf.len() as _)
            .flags(flags)
            .build();

        let res = Proactor::get()
            .inner()
            .register_io(sqe)?
            .await?;

        Ok(res as _)
    }

    ///////////////////////////////////
    ///// Connect
    ///// Commonality of TcpStream, UdpSocket
    ///////////////////////////////////

    pub(crate) async fn processor_connect<A: ToSocketAddrs, F, Fut, T>(
        addrs: A,
        mut f: F,
    ) -> io::Result<T>
    where
        F: FnMut(SocketAddr) -> Fut,
        Fut: Future<Output = io::Result<T>>,
    {
        // TODO connect_tcp, connect_udp
        let addrs = match addrs.to_socket_addrs() {
            Ok(addrs) => addrs,
            Err(e) => return Err(e),
        };

        let mut tail_err = None;
        for addr in addrs {
            match f(addr).await {
                Ok(l) => return Ok(l),
                Err(e) => tail_err = Some(e),
            }
        }

        Err(tail_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "couldn't resolve addresses")
        }))
    }

    pub(crate) async fn processor_connect_tcp(addr: SocketAddr) -> io::Result<Handle<TcpStream>> {
        let addr = addr.to_string();
        // FIXME: address resolution is always blocking.
        let addr: SocketAddr = addr.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "could not resolve the address")
        })?;

        let domain = if addr.is_ipv6() {
            socket2::Domain::ipv6()
        } else {
            socket2::Domain::ipv4()
        };

        let sock = socket2::Socket::new(
            domain,
            socket2::Type::stream(),
            Some(socket2::Protocol::tcp()),
        )?;

        sock.set_nonblocking(true)?;

        let nixsaddr = SockAddr::from(addr);

        let mut stream = sock.into_tcp_stream();
        stream.set_nodelay(true)?;
        let fd = stream.as_raw_fd() as _;

        let mut ossa: OsSocketAddr = addr.into();
        let socklen = ossa.len();

        let mut sqe = OP::Connect::new(Fd(fd), unsafe{ std::mem::transmute(ossa.as_ptr()) }, socklen)
            .build();

        Proactor::get()
            .inner()
            .register_io(sqe)?
            .await?;

        Ok(Handle::new(stream)?)
    }

    pub(crate) async fn processor_connect_udp(addr: SocketAddr) -> io::Result<Handle<UdpSocket>> {
        let domain = match addr {
            SocketAddr::V4(_) => socket2::Domain::ipv4(),
            SocketAddr::V6(_) => socket2::Domain::ipv6(),
        };
        let sock = socket2::Socket::new(
            domain,
            socket2::Type::dgram(),
            Some(socket2::Protocol::udp()),
        )?;
        let sockaddr = socket2::SockAddr::from(addr);

        let unspec = match addr {
            SocketAddr::V4(_) => {
                let unspecv4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
                socket2::SockAddr::from(unspecv4)
            }
            SocketAddr::V6(_) => {
                let unspecv6 = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
                socket2::SockAddr::from(unspecv6)
            }
        };

        // Try to bind to the datagram socket.
        sock.bind(&unspec)?;
        sock.set_nonblocking(true)?;

        // Try to connect over the socket
        sock.connect(&sockaddr)?;

        // Make into udp type and init handler.
        Ok(Handle::new(sock.into_udp_socket())?)
    }

    ///////////////////////////////////
    ///// TcpListener
    ///////////////////////////////////

    pub(crate) async fn processor_accept_tcp_listener<R: AsRawFd>(
        listener: &R
    ) -> io::Result<(Handle<TcpStream>, Option<SocketAddr>)> {
        let fd = listener.as_raw_fd() as _;

        let (mut storage, mut addrlen) = unsafe {
            let mut addrlen = mem::size_of::<sockaddr>() as socklen_t;
            let mut storage = MaybeUninit::<sockaddr>::zeroed().assume_init();
            (storage, addrlen)
        };

        let mut sqe = OP::Accept::new(Fd(fd), &mut storage as *mut _ as *mut _, &mut addrlen as *mut _ as *mut _)
            .flags(SocketFlags::NONBLOCK)
            .build();

        let cc = Proactor::get().inner().register_io(sqe)?;
        let stream = unsafe { TcpStream::from_raw_fd(cc.await?) };

        Ok((Handle::new(stream).unwrap(), None))
    }

    ///////////////////////////////////
    ///// UdpSocket
    ///////////////////////////////////

    pub(crate) async fn processor_send_to<R: AsRawFd>(
        socket: &R,
        buf: &[u8],
        addr: SocketAddr,
    ) -> io::Result<usize> {
        Self::send_to_dest(socket, buf, &socket2::SockAddr::from(addr)).await
    }

    async fn send_to_dest<A: AsRawFd>(
        socket: &A,
        buf: &[u8],
        addr: &socket2::SockAddr,
    ) -> io::Result<usize> {
        // FIXME: (vcq): Wrap into vec?
        let mut iov = IoSlice::new(buf);

        let mut sendmsg = unsafe { MaybeUninit::<msghdr>::zeroed().assume_init() };
        sendmsg.msg_name = addr.as_ptr() as *mut _;
        sendmsg.msg_namelen = addr.len() as _;
        sendmsg.msg_iov = iov.as_ptr() as *mut _;
        sendmsg.msg_iovlen = iov.len();

        let fd = socket.as_raw_fd() as _;

        let mut sqe = OP::SendMsg::new(Fd(fd), &sendmsg as *const _ as *const _)
            .flags(SendFlags::empty())
            .build();

        let res = Proactor::get()
            .inner()
            .register_io(sqe)?
            .await?;

        Ok(res as _)
    }

    pub(crate) async fn processor_recv_from<R: AsRawFd>(
        sock: &R,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        Self::recv_from_with_flags(sock, buf, RecvFlags::empty())
            .await
            .map(|(size, sockaddr)| (size, sockaddr.as_std().unwrap()))
    }

    pub(crate) async fn processor_peek_from<R: AsRawFd>(
        sock: &R,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        Self::recv_from_with_flags(sock, buf, RecvFlags::PEEK)
            .await
            .map(|(size, sockaddr)| (size, sockaddr.as_std().unwrap()))
    }

    async fn recv_from_with_flags<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
        flags: RecvFlags,
    ) -> io::Result<(usize, socket2::SockAddr)> {
        let mut sockaddr_raw =
            unsafe { MaybeUninit::<libc::sockaddr_storage>::zeroed().assume_init() };

        // FIXME: (vcq): Wrap into vec?
        let mut iov = IoSliceMut::new(buf);

        let mut recvmsg = unsafe { MaybeUninit::<msghdr>::zeroed().assume_init() };
        recvmsg.msg_name = &mut sockaddr_raw as *mut _ as _;
        recvmsg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as _;
        recvmsg.msg_iov = iov.as_ptr() as *mut _;
        recvmsg.msg_iovlen = iov.len();

        let fd = socket.as_raw_fd() as _;

        let mut sqe = OP::RecvMsg::new(Fd(fd), &mut recvmsg as *mut _ as *mut _)
            .flags(flags)
            .ioprio((IoringRecvFlags::POLL_FIRST | IoringRecvFlags::MULTISHOT).bits())
            .build();

        let res = Proactor::get()
            .inner()
            .register_io(sqe)?
            .await?;

        let sockaddr = unsafe {
            socket2::SockAddr::from_raw_parts(
                &sockaddr_raw as *const _ as *const _,
                recvmsg.msg_namelen as _,
            )
        };

        Ok((res as _, sockaddr))
    }

    ///////////////////////////////////
    ///// UnixListener
    ///////////////////////////////////

    pub(crate) async fn processor_accept_unix_listener<R: AsRawFd>(
        listener: &R,
    ) -> io::Result<(Handle<UnixStream>, UnixSocketAddr)> {
        let fd = listener.as_raw_fd() as _;
        let sockfd: OwnedFd = unsafe { OwnedFd::from_raw_fd(fd) };
        let sockaddr = match rustix::net::getsockname(&sockfd)? {
            SocketAddrAny::Unix(sockaddr) => sockaddr,
            _ => return Err(io::Error::last_os_error())
        };
        let mut natsockaddr: ShimSocketAddrUnix = unsafe { std::mem::transmute(sockaddr) };

        let mut sqe = OP::Accept::new(Fd(fd), &mut natsockaddr.unix as *mut _ as *mut _, natsockaddr.len as _)
            .flags(SocketFlags::empty())
            .build();

        let cc = Proactor::get().inner().register_io(sqe)?;

        let stream = unsafe { UnixStream::from_raw_fd(cc.await?) };
        let usa = unsafe { socket2::SockAddr::from_raw_parts(
            &natsockaddr.unix as *const _ as *const _, natsockaddr.len as _
        ) };
        let addr = shim_to_af_unix(&usa)?;

        Ok((Handle::new(stream)?, addr))
    }

    ///////////////////////////////////
    ///// UnixStream
    ///////////////////////////////////

    pub(crate) async fn processor_connect_unix<P: AsRef<Path>>(
        path: P,
    ) -> io::Result<Handle<UnixStream>> {
        let sock = socket2::Socket::new(socket2::Domain::unix(), socket2::Type::stream(), None)?;
        // let sockaddr = socket2::SockAddr::unix(path)?;
        let sockaddr = SocketAddrUnix::new(path.as_ref())?;
        let mut sockaddr: ShimSocketAddrUnix = unsafe { std::mem::transmute(sockaddr) };

        sock.set_nonblocking(true)?;

        let stream: UnixStream = sock.into_unix_stream();
        let fd = stream.as_raw_fd() as _;

        let mut sqe = OP::Connect::new(Fd(fd), &sockaddr.unix as *const _ as *const _, sockaddr.len)
            .build();

        Proactor::get()
            .inner()
            .register_io(sqe)?
            .await?;

        Ok(Handle::new(stream)?)
    }

    pub(crate) async fn processor_send_to_unix<R: AsRawFd, P: AsRef<Path>>(
        socket: &R,
        buf: &[u8],
        path: P,
    ) -> io::Result<usize> {
        Self::send_to_dest(socket, buf, &socket2::SockAddr::unix(path)?).await
    }

    pub(crate) async fn processor_recv_from_unix<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
    ) -> io::Result<(usize, UnixSocketAddr)> {
        Self::recv_from_with_flags(socket, buf, RecvFlags::empty())
            .await
            .map(|(size, sockaddr)| (size, shim_to_af_unix(&sockaddr).unwrap()))
    }

    pub(crate) async fn processor_peek_from_unix<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
    ) -> io::Result<(usize, UnixSocketAddr)> {
        Self::recv_from_with_flags(socket, buf, RecvFlags::PEEK)
            .await
            .map(|(size, sockaddr)| (size, shim_to_af_unix(&sockaddr).unwrap()))
    }
}

#[derive(Clone)]
pub struct ShimSocketAddrUnix {
    pub unix: sockaddr_un,
    pub len: socklen_t,
}