use std::future::Future;
use std::io;
use std::marker::Unpin;
use std::sync::Arc;
use std::path::Path;
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::unix::io::AsRawFd;

use std::net::{TcpListener, TcpStream, UdpSocket};
// Unix specifics
use std::os::unix::net::{UnixListener, UnixStream, SocketAddr as UnixSocketAddr};

use lever::sync::prelude::*;
use futures::Stream;

use crate::{Handle, Proactor};
use super::Processor;
use crate::syscore::CompletionChan;


impl<T: AsRawFd> Handle<T> {
    pub fn new(io: T) -> io::Result<Handle<T>> {
        Ok(Handle {
            io_task: Some(io),
            chan: None,
            read: Arc::new(TTas::new(None)),
            write: Arc::new(TTas::new(None)),
        })
    }

    pub(crate) fn new_with_callback(io: T, evflags: usize) -> io::Result<Handle<T>> {
        let fd = io.as_raw_fd();
        let mut handle = Handle::new(io)?;
        let register = Proactor::get()
            .inner()
            .register_io(fd, evflags)?;
        handle.chan = Some(register);
        Ok(handle)
    }
}

impl Handle<TcpListener> {
    pub fn new_socket(io: TcpListener) -> io::Result<Handle<TcpListener>> {
        Handle::new(io)
    }

    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Handle<TcpListener>> {
        // TODO: (vertexclique): Migrate towards to using initial subscription with callback.
        // Using `new_with_callback`.
        Ok(Handle::new(TcpListener::bind(addr)?)?)
    }

    pub async fn accept(&self) -> io::Result<(Handle<TcpStream>, SocketAddr)> {
        Processor::processor_accept_tcp_listener(self.get_ref()).await
    }

    pub fn incoming(
        &self,
    ) -> impl Stream<Item = io::Result<Handle<TcpStream>>> + Send + Unpin + '_ {
        Box::pin(futures::stream::unfold(self, |listener: &Handle<TcpListener>| async move {
            let res = listener.accept().await.map(|(stream, _)| stream);
            Some((res, listener))
        }))
    }
}

impl Handle<TcpStream> {
    pub async fn connect<A: ToSocketAddrs>(addrs: A) -> io::Result<Handle<TcpStream>> {
        Ok(Processor::processor_connect(addrs, Processor::processor_connect_tcp).await?)
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        Processor::processor_send(self.get_ref(), buf).await
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        Processor::processor_recv(self.get_ref(), buf).await
    }

    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        Processor::processor_peek(self.get_ref(), buf).await
    }
}

impl Handle<UdpSocket> {
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Handle<UdpSocket>> {
        Ok(Handle::new(UdpSocket::bind(addr)?)?)
    }

    pub async fn connect<A: ToSocketAddrs>(sock_addrs: A) -> io::Result<Handle<UdpSocket>> {
        Processor::processor_connect(sock_addrs, Processor::processor_connect_udp).await
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        Processor::processor_send(self.get_ref(), buf).await
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        Processor::processor_recv(self.get_ref(), buf).await
    }

    pub async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        Processor::processor_peek(self.get_ref(), buf).await
    }

    pub async fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> io::Result<usize> {
        match addr.to_socket_addrs()?.next() {
            Some(addr) =>
                Processor::processor_send_to(self.get_ref(), buf, addr).await,
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "given addresses can't be parsed",
            )),
        }
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        Processor::processor_recv_from(self.get_ref(), buf).await
    }

    pub async fn peek_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        Processor::processor_peek_from(self.get_ref(), buf).await
    }
}

impl Handle<UnixListener> {
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Handle<UnixListener>> {
        Ok(Handle::new(UnixListener::bind(path)?)?)
    }

    pub async fn accept(&self) -> io::Result<(Handle<UnixStream>, UnixSocketAddr)> {
        Processor::processor_accept_unix_listener(self.get_ref()).await
    }

    pub fn incoming(
        &self,
    ) -> impl Stream<Item = io::Result<Handle<UnixStream>>> + Send + Unpin + '_ {
        Box::pin(futures::stream::unfold(self, |listener: &Handle<UnixListener>| async move {
            let res = listener.accept().await.map(|(stream, _)| stream);
            Some((res, listener))
        }))
    }
}