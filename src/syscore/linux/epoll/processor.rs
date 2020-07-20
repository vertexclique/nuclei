use std::future::Future;
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
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
use crate::syscore::shim_to_af_unix;
use crate::Handle;

pub struct Processor;

impl Processor {
    ///////////////////////////////////
    ///// Read Write Seek
    ///// Synchronous File
    ///////////////////////////////////

    pub(crate) async fn processor_read_file<R: AsRawFd>(
        io: &R,
        buf: &mut [u8],
    ) -> io::Result<usize> {
        // TODO: (vertexclique): Use blocking here.
        let mut file = unsafe { File::from_raw_fd(io.as_raw_fd()) };
        let res = file.read(buf);
        let _ = ManuallyDrop::new(file);
        res
    }

    pub(crate) async fn processor_write_file<R: AsRawFd>(io: &R, buf: &[u8]) -> io::Result<usize> {
        // TODO: (vertexclique): Use blocking here.
        let mut file = unsafe { File::from_raw_fd(io.as_raw_fd()) };
        let res = file.write(buf);

        let _ = ManuallyDrop::new(file);
        res
    }

    pub(crate) async fn processor_seek_file<R: AsRawFd>(
        io: &R,
        pos: SeekFrom,
    ) -> io::Result<usize> {
        // TODO: (vertexclique): Use blocking here or in the outer environment.
        let mut file = unsafe { File::from_raw_fd(io.as_raw_fd()) };
        let res = file.seek(pos);

        let _ = ManuallyDrop::new(file);
        res.map(|e| e as usize)
    }

    ///////////////////////////////////
    ///// Send, Recv, Peek
    ///// Commonality of TcpStream, UdpSocket, UnixStream, UnixDatagram
    ///////////////////////////////////

    pub(crate) async fn processor_send<R: AsRawFd>(socket: &R, buf: &[u8]) -> io::Result<usize> {
        let sock = unsafe { socket2::Socket::from_raw_fd(socket.as_raw_fd()) };
        let sock = ManuallyDrop::new(sock);

        // Reregister on block
        match sock.send(buf) {
            Ok(res) => Ok(res),
            Err(err) if (err.raw_os_error().unwrap() & (libc::EAGAIN | libc::EWOULDBLOCK)) != 0 => {
                let cc = Proactor::get()
                    .inner()
                    .register_io(socket.as_raw_fd(), libc::EPOLLIN as i32)?;
                let events = cc.await?;
                if (events & libc::EPOLLERR as i32) != 0 {
                    Err(sock.take_error()?.unwrap())
                } else {
                    sock.send(buf)
                }
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn processor_recv<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
        Self::recv_with_flags(sock, buf, 0).await
    }

    pub(crate) async fn processor_peek<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
        Self::recv_with_flags(sock, buf, libc::MSG_PEEK as _).await
    }

    async fn recv_with_flags<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
        flags: u32,
    ) -> io::Result<usize> {
        let sock = unsafe { socket2::Socket::from_raw_fd(socket.as_raw_fd()) };
        let sock = ManuallyDrop::new(sock);

        // Reregister on block
        match sock.recv_with_flags(buf, flags as _) {
            Ok(res) => Ok(res),
            Err(err) if (err.raw_os_error().unwrap() & (libc::EAGAIN | libc::EWOULDBLOCK)) != 0 => {
                let cc = Proactor::get()
                    .inner()
                    .register_io(socket.as_raw_fd(), libc::EPOLLIN as _)?;
                let events = cc.await?;
                if (events & libc::EPOLLERR as i32) != 0 {
                    Err(sock.take_error()?.unwrap())
                } else {
                    sock.recv_with_flags(buf, flags as _)
                }
            }
            Err(e) => Err(e),
        }
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
        let addr = addr.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "could not resolve the address")
        })?;

        // Create a socket.
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

        // Begin async connect and ignore the inevitable "in progress" error.
        sock.set_nonblocking(true)?;
        sock.connect(&addr.into()).or_else(|err| {
            let in_progress = err.raw_os_error() == Some(libc::EINPROGRESS);

            // If connect results with an "in progress" error, that's not an error.
            if in_progress {
                Ok(())
            } else {
                Err(err)
            }
        })?;

        let mut stream_raw = sock.into_tcp_stream();
        stream_raw.set_nodelay(true)?;

        let stream = Handle::new(stream_raw)?;

        // TODO: Recurse here on connect purpose

        match stream.get_ref().take_error()? {
            None => Ok(stream),
            Some(err) => Err(err),
        }
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
        listener: &R,
    ) -> io::Result<(Handle<TcpStream>, SocketAddr)> {
        let socket = unsafe { socket2::Socket::from_raw_fd(listener.as_raw_fd()) };
        let socket = socket.into_tcp_listener();
        let socket = ManuallyDrop::new(socket);

        // Reregister on block
        match socket
            .accept()
            .map(|(stream, sockaddr)| (Handle::new(stream).unwrap(), sockaddr))
        {
            Ok(res) => Ok(res),
            Err(err) if (err.raw_os_error().unwrap() & (libc::EAGAIN | libc::EWOULDBLOCK)) != 0 => {
                let cc = Proactor::get()
                    .inner()
                    .register_io(listener.as_raw_fd(), libc::EPOLLIN as _)?;
                let events = cc.await?;
                if (events & libc::EPOLLERR as i32) != 0 {
                    Err(socket.take_error()?.unwrap())
                } else {
                    socket
                        .accept()
                        .map(|(stream, sockaddr)| (Handle::new(stream).unwrap(), sockaddr))
                }
            }
            Err(e) => Err(e),
        }
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
        let sock = unsafe { socket2::Socket::from_raw_fd(socket.as_raw_fd()) };
        let sock = ManuallyDrop::new(sock);

        // Reregister on block
        match sock.send_to(buf, addr) {
            Ok(res) => Ok(res),
            Err(err) if (err.raw_os_error().unwrap() & (libc::EAGAIN | libc::EWOULDBLOCK)) != 0 => {
                let cc = Proactor::get()
                    .inner()
                    .register_io(socket.as_raw_fd(), libc::EPOLLIN as _)?;
                let events = cc.await?;
                if (events & libc::EPOLLERR as i32) != 0 {
                    Err(sock.take_error()?.unwrap())
                } else {
                    sock.send_to(buf, addr)
                }
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn processor_recv_from<R: AsRawFd>(
        sock: &R,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        Self::recv_from_with_flags(sock, buf, 0)
            .await
            .map(|(size, sockaddr)| (size, sockaddr.as_std().unwrap()))
    }

    pub(crate) async fn processor_peek_from<R: AsRawFd>(
        sock: &R,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        Self::recv_from_with_flags(sock, buf, libc::MSG_PEEK as _)
            .await
            .map(|(size, sockaddr)| (size, sockaddr.as_std().unwrap()))
    }

    async fn recv_from_with_flags<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
        flags: u32,
    ) -> io::Result<(usize, socket2::SockAddr)> {
        let sock = unsafe { socket2::Socket::from_raw_fd(socket.as_raw_fd()) };
        // let sockpass = unsafe { socket2::Socket::from_raw_fd(socket.as_raw_fd()) };
        // let sock = ManuallyDrop::new(sock);

        // Reregister on block
        match super::shim_recv_from(sock, buf, flags as _).map(|(size, sockaddr)| (size, sockaddr))
        {
            Ok(res) => Ok(res),
            Err(err) if (err.raw_os_error().unwrap() & (libc::EAGAIN | libc::EWOULDBLOCK)) != 0 => {
                let cc = Proactor::get()
                    .inner()
                    .register_io(socket.as_raw_fd(), libc::EPOLLIN as _)?;
                let events = cc.await?;
                if (events & libc::EPOLLERR as i32) != 0 {
                    let sock = unsafe { socket2::Socket::from_raw_fd(socket.as_raw_fd()) };
                    let sock = ManuallyDrop::new(sock);
                    Err(sock.take_error()?.unwrap())
                } else {
                    let sock = unsafe { socket2::Socket::from_raw_fd(socket.as_raw_fd()) };
                    super::shim_recv_from(sock, buf, flags as _)
                        .map(|(size, sockaddr)| (size, sockaddr))
                }
            }
            Err(e) => Err(e),
        }
    }

    ///////////////////////////////////
    ///// UnixListener
    ///////////////////////////////////

    pub(crate) async fn processor_accept_unix_listener<R: AsRawFd>(
        listener: &R,
    ) -> io::Result<(Handle<UnixStream>, UnixSocketAddr)> {
        let socket = unsafe { socket2::Socket::from_raw_fd(listener.as_raw_fd()) };
        let socket = socket.into_unix_listener();
        let socket = ManuallyDrop::new(socket);

        // Reregister on block
        match socket
            .accept()
            .map(|(stream, sockaddr)| (Handle::new(stream).unwrap(), sockaddr))
        {
            Ok(res) => Ok(res),
            Err(err) if (err.raw_os_error().unwrap() & (libc::EAGAIN | libc::EWOULDBLOCK)) != 0 => {
                let cc = Proactor::get()
                    .inner()
                    .register_io(socket.as_raw_fd(), libc::EPOLLIN as _)?;
                let events = cc.await?;
                if (events & libc::EPOLLERR as i32) != 0 {
                    Err(socket.take_error()?.unwrap())
                } else {
                    socket
                        .accept()
                        .map(|(stream, sockaddr)| (Handle::new(stream).unwrap(), sockaddr))
                }
            }
            Err(e) => Err(e),
        }
    }

    ///////////////////////////////////
    ///// UnixStream
    ///////////////////////////////////

    pub(crate) async fn processor_connect_unix<P: AsRef<Path>>(
        path: P,
    ) -> io::Result<Handle<UnixStream>> {
        let sock = socket2::Socket::new(socket2::Domain::unix(), socket2::Type::stream(), None)?;
        let sockaddr = socket2::SockAddr::unix(path)?;

        sock.set_nonblocking(true)?;

        let stream = Handle::new(sock.into_unix_stream())?;

        let sock = unsafe { socket2::Socket::from_raw_fd(stream.as_raw_fd()) };
        let sock = ManuallyDrop::new(sock);

        let res = match sock.connect(&sockaddr) {
            Ok(res) => Ok(res),
            Err(err) if (err.raw_os_error().unwrap() & (libc::EAGAIN | libc::EINPROGRESS)) != 0 => {
                let cc = Proactor::get()
                    .inner()
                    .register_io(stream.as_raw_fd(), libc::EPOLLOUT as _)?;
                let events = cc.await?;
                if (events & libc::EPOLLERR as i32) != 0 {
                    Err(sock.take_error()?.unwrap())
                } else {
                    sock.connect(&sockaddr)
                }
            }
            Err(e) => Err(e),
        };

        res.map(|_| stream)
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
        Self::recv_from_with_flags(socket, buf, 0)
            .await
            .map(|(size, sockaddr)| (size, shim_to_af_unix(&sockaddr).unwrap()))
    }

    pub(crate) async fn processor_peek_from_unix<R: AsRawFd>(
        socket: &R,
        buf: &mut [u8],
    ) -> io::Result<(usize, UnixSocketAddr)> {
        Self::recv_from_with_flags(socket, buf, libc::MSG_PEEK as _)
            .await
            .map(|(size, sockaddr)| (size, shim_to_af_unix(&sockaddr).unwrap()))
    }
}
