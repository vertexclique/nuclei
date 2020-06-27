use std::io;
use std::io::{Read, Write};
use std::{fs::File, os::unix::io::{AsRawFd, FromRawFd}, mem::ManuallyDrop};
use std::net::{SocketAddr, ToSocketAddrs, TcpListener};
use std::os::unix::net::{SocketAddr as UnixSocketAddr, UnixDatagram, UnixListener, UnixStream};
use std::future::Future;
use std::path::Path;
use std::net::TcpStream;

use crate::proactor::Proactor;
use crate::Handle;

pub struct Processor;

impl Processor {
    ///////////////////////////////////
    ///// Read Write
    ///// Synchronous File
    ///////////////////////////////////

    pub(crate) async fn processor_read_file<R: AsRawFd>(io: &R, buf: &mut [u8]) -> io::Result<usize> {
        let mut file = unsafe { File::from_raw_fd(io.as_raw_fd()) };
        let res = file.read(buf);
        let _ = ManuallyDrop::new(file);
        res
    }

    pub(crate) async fn processor_write_file<R: AsRawFd>(io: &R, buf: &[u8]) -> io::Result<usize> {
        let mut file = unsafe { File::from_raw_fd(io.as_raw_fd()) };
        let res = file.write(buf);

        let _ = ManuallyDrop::new(file);
        res
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
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let notifier = Proactor::get()
                    .inner()
                    .register_io(socket.as_raw_fd(), libc::EVFILT_WRITE as _)?;
                let events = notifier.await?;
                if events & (libc::EV_ERROR as usize) != 0 {
                    // FIXME: (vertexclique): Surely this won't happen, since it is filtered in the evloop.
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
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let notifier = Proactor::get()
                    .inner()
                    .register_io(socket.as_raw_fd(), libc::EVFILT_READ as _)?;
                let events = notifier.await?;
                if events & (libc::EV_ERROR as usize) != 0 {
                    // FIXME: (vertexclique): Surely this won't happen, since it is filtered in the evloop.
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

    pub(crate) async fn processor_connect<A: ToSocketAddrs, F, Fut, T>(addrs: A, mut f: F) -> io::Result<T>
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
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "couldn't resolve addresses",
            )
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
        let sock = socket2::Socket::new(domain, socket2::Type::stream(), Some(socket2::Protocol::tcp()))?;

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

        let stream = Handle::new(sock.into_tcp_stream())?;

        // create temp socket for connect purpose.
        let sock = unsafe { socket2::Socket::from_raw_fd(stream.as_raw_fd()) };
        let sock = ManuallyDrop::new(sock);

        match stream.get_ref().take_error()? {
            None => Ok(stream),
            Some(err) => Err(err),
        }
    }

    ///////////////////////////////////
    ///// TcpListener
    ///////////////////////////////////

    pub(crate) async fn processor_accept_tcp_listener<R: AsRawFd>(listener: &R) -> io::Result<(Handle<TcpStream>, SocketAddr)> {
        let socket = unsafe { socket2::Socket::from_raw_fd(listener.as_raw_fd()) };
        let socket = socket.into_tcp_listener();
        let socket = ManuallyDrop::new(socket);

        // Reregister on block
        match socket
                .accept()
                .map(|(stream, sockaddr)| (Handle::new(stream).unwrap(), sockaddr)) {
            Ok(res) => Ok(res),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let notifier = Proactor::get()
                    .inner()
                    .register_io(listener.as_raw_fd(), libc::EVFILT_READ as _)?;
                let events = notifier.await?;
                if events & (libc::EV_ERROR as usize) != 0 {
                    // FIXME: (vertexclique): Surely this won't happen, since it is filtered in the evloop.
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

    async fn send_to_dest<A: AsRawFd>(socket: &A, buf: &[u8], addr: &socket2::SockAddr) -> io::Result<usize> {
        let sock = unsafe { socket2::Socket::from_raw_fd(socket.as_raw_fd()) };
        let sock = ManuallyDrop::new(sock);

        // Reregister on block
        match sock.send_to(buf, addr) {
            Ok(res) => Ok(res),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let notifier = Proactor::get()
                    .inner()
                    .register_io(socket.as_raw_fd(), libc::EVFILT_READ as _)?;
                let events = notifier.await?;
                if events & (libc::EV_ERROR as usize) != 0 {
                    // FIXME: (vertexclique): Surely this won't happen, since it is filtered in the evloop.
                    Err(sock.take_error()?.unwrap())
                } else {
                    sock.send_to(buf, addr)
                }
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn processor_recv_from<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        Self::recv_from_with_flags(sock, buf, 0)
            .await
            .map(|(size, sockaddr)| (size, sockaddr.as_std().unwrap()))
    }

    pub(crate) async fn processor_peek_from<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
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
        match super::shim_recv_from(sock, buf, flags as _)
            .map(|(size, sockaddr)| (size, sockaddr)) {
            Ok(res) => Ok(res),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let notifier = Proactor::get()
                    .inner()
                    .register_io(socket.as_raw_fd(), libc::EVFILT_READ as _)?;
                let events = notifier.await?;
                if events & (libc::EV_ERROR as usize) != 0 {
                    // FIXME: (vertexclique): Surely this won't happen, since it is filtered in the evloop.
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

    pub(crate) async fn processor_accept_unix_listener(&self) -> io::Result<(Handle<UnixStream>, UnixSocketAddr)> {
        todo!()
    }

    ///////////////////////////////////
    ///// UnixStream
    ///////////////////////////////////

    pub(crate) async fn processor_connect_unix<P: AsRef<Path>>(path: P) -> io::Result<Handle<UnixStream>> {
        todo!()
    }

    pub(crate) async fn processor_send_to_unix<P: AsRef<Path>>(&self, buf: &[u8], path: P) -> io::Result<usize> {
        todo!()
    }

    pub(crate) async fn processor_recv_from_unix(&self, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        todo!()
    }

    pub(crate) async fn processor_peek_from_unix(&self, buf: &mut [u8]) -> io::Result<(usize, UnixSocketAddr)> {
        todo!()
    }
}