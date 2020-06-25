use std::io;
use std::io::{Read, Write};
use std::{fs::File, os::unix::io::{AsRawFd, FromRawFd}, mem::ManuallyDrop};
use std::net::{SocketAddr, ToSocketAddrs};
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
                    // Surely this won't happen, since it is filtered in the evloop.
                    Err(sock.take_error()?.unwrap())
                } else {
                    sock.send(buf)
                }
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn processor_recv<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
        todo!()
    }

    pub(crate) async fn processor_peek<R: AsRawFd>(sock: &R, buf: &mut [u8]) -> io::Result<usize> {
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

    pub(crate) async fn processor_accept_tcp_listener<R: AsRawFd>(io: &R, buf: &[u8]) -> io::Result<usize> {
        todo!()
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

    pub(crate) async fn processor_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        todo!()
    }

    pub(crate) async fn processor_peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        todo!()
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