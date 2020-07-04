use std::io;
use std::io::{Read, Write};
use std::{fs::File, os::unix::io::{AsRawFd, FromRawFd}, mem::ManuallyDrop};
use std::net::{SocketAddr, ToSocketAddrs, TcpListener};
use std::os::unix::net::{
    SocketAddr as UnixSocketAddr, UnixDatagram, UnixListener, UnixStream,
};
use std::net::{SocketAddrV6, SocketAddrV4, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::future::Future;
use std::path::Path;
use std::net::TcpStream;

use crate::proactor::Proactor;
use crate::Handle;
use crate::syscore::shim_to_af_unix;
use std::io::{IoSliceMut, IoSlice};
use iou::{SockFlag, SockAddrStorage};


pub struct Processor;

impl Processor {
    ///////////////////////////////////
    ///// Read Write
    ///// Synchronous File
    ///////////////////////////////////

    pub(crate) async fn processor_read_file<R: AsRawFd>(io: &R, buf: &mut [u8]) -> io::Result<usize> {
        let fd = io.as_raw_fd() as _;

        let cc = Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_read_vectored(fd, &mut [IoSliceMut::new(buf)], 0);
        })?;

        Ok(cc.await? as _)
    }

    pub(crate) async fn processor_write_file<R: AsRawFd>(io: &R, buf: &[u8]) -> io::Result<usize> {
        let fd = io.as_raw_fd() as _;

        let cc = Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_write_vectored(fd, &mut [IoSlice::new(buf)], 0);
        })?;

        Ok(cc.await? as _)
    }

    ///////////////////////////////////
    ///// Send, Recv, Peek
    ///// Commonality of TcpStream, UdpSocket, UnixStream, UnixDatagram
    ///////////////////////////////////

    pub(crate) async fn processor_send<R: AsRawFd>(socket: &R, buf: &[u8]) -> io::Result<usize> {
        todo!()
    }

    pub(crate) async fn processor_recv<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
        todo!()
    }

    pub(crate) async fn processor_peek<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
        todo!()
    }

    async fn recv_with_flags<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
        flags: u32,
    ) -> io::Result<usize> {
        todo!()
    }

    ///////////////////////////////////
    ///// Connect
    ///// Commonality of TcpStream, UdpSocket
    ///////////////////////////////////

    pub(crate) async fn processor_connect<A: ToSocketAddrs, F, Fut, T>(addrs: A, mut f: F) -> io::Result<T>
        where
            F: FnMut(SocketAddr) -> Fut,
            Fut: Future<Output = io::Result<T>>,
    {
        todo!()
    }

    pub(crate) async fn processor_connect_tcp(addr: SocketAddr) -> io::Result<Handle<TcpStream>> {
        let addr = addr.to_string();
        // FIXME: address resolution is always blocking.
        let addr = addr.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "could not resolve the address")
        })?;

        let domain = if addr.is_ipv6() {
            socket2::Domain::ipv6()
        } else {
            socket2::Domain::ipv4()
        };
        let sock = socket2::Socket::new(domain, socket2::Type::stream(), Some(socket2::Protocol::tcp()))?;

        sock.set_nonblocking(true)?;

        // FIXME: (vcq): iou uses nix, i use socket2, conversions happens over libc.
        // Propose std conversion for nix.
        let nixsaddr =
            unsafe {
                &iou::SockAddr::from_libc_sockaddr(sock.local_addr().unwrap().as_ptr()).unwrap()
            };
        let stream = sock.into_tcp_stream();
        let fd = stream.as_raw_fd() as _;

        Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_connect(fd, nixsaddr);
        })?.await?;

        Ok(Handle::new(stream)?)
    }

    pub(crate) async fn processor_connect_udp(addr: SocketAddr) -> io::Result<Handle<UdpSocket>> {
        todo!()
    }

    ///////////////////////////////////
    ///// TcpListener
    ///////////////////////////////////

    pub(crate) async fn processor_accept_tcp_listener<R: AsRawFd>(listener: &R) -> io::Result<(Handle<TcpStream>, SocketAddr)> {
        let fd = listener.as_raw_fd() as _;
        let mut saddrstor = SockAddrStorage::uninit();

        let cc = Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_accept(fd, Some(&mut saddrstor), SockFlag::empty())
        })?;

        let stream = unsafe { TcpStream::from_raw_fd(cc.await?) };
        let addr = unsafe {
            let nixsa = saddrstor.as_socket_addr()?;
            let (saddr, saddr_len) = nixsa.as_ffi_pair();
            socket2::SockAddr::from_raw_parts(saddr as *const _, saddr_len as _)
                .as_std()
                .unwrap()
        };

        Ok((Handle::new(stream)?, addr))
    }

    ///////////////////////////////////
    ///// UdpSocket
    ///////////////////////////////////

    pub(crate) async fn processor_send_to<R: AsRawFd>(
        socket: &R,
        buf: &[u8],
        addr: SocketAddr,
    ) -> io::Result<usize> {
        todo!()
    }

    async fn send_to_dest<A: AsRawFd>(socket: &A, buf: &[u8], addr: &socket2::SockAddr) -> io::Result<usize> {
        todo!()
    }

    pub(crate) async fn processor_recv_from<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        todo!()
    }

    pub(crate) async fn processor_peek_from<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        todo!()
    }

    async fn recv_from_with_flags<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
        flags: u32,
    ) -> io::Result<(usize, socket2::SockAddr)> {
        todo!()
    }

    ///////////////////////////////////
    ///// UnixListener
    ///////////////////////////////////

    pub(crate) async fn processor_accept_unix_listener<R: AsRawFd>(listener: &R) -> io::Result<(Handle<UnixStream>, UnixSocketAddr)> {
        let fd = listener.as_raw_fd() as _;
        let mut saddrstor = SockAddrStorage::uninit();

        let cc = Proactor::get().inner().register_io(|sqe| unsafe {
            sqe.prep_accept(fd, Some(&mut saddrstor), SockFlag::empty())
        })?;

        let stream = unsafe { UnixStream::from_raw_fd(cc.await?) };
        let addr = unsafe {
            let nixsa = saddrstor.as_socket_addr()?;
            let (saddr, saddr_len) = nixsa.as_ffi_pair();
            socket2::SockAddr::from_raw_parts(saddr as *const _, saddr_len as _)
        };
        let addr = shim_to_af_unix(&addr)?;

        Ok((Handle::new(stream)?, addr))
    }

    ///////////////////////////////////
    ///// UnixStream
    ///////////////////////////////////

    pub(crate) async fn processor_connect_unix<P: AsRef<Path>>(path: P) -> io::Result<Handle<UnixStream>> {
        todo!()
    }

    pub(crate) async fn processor_send_to_unix<R: AsRawFd, P: AsRef<Path>>(socket: &R, buf: &[u8], path: P) -> io::Result<usize> {
        todo!()
    }

    pub(crate) async fn processor_recv_from_unix<R: AsRawFd>(socket: &R, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        todo!()
    }

    pub(crate) async fn processor_peek_from_unix<R: AsRawFd>(socket: &R, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        todo!()
    }
}