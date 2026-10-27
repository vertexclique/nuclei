//! A socket's calls of its caller's own (`try_io`, `async_io`): raw calls
//! on the descriptor once the socket may be ready, in order with the
//! handle's own reads and writes, on each backend.
#![cfg(unix)]

use std::io;
use std::os::fd::AsRawFd;
use std::time::Duration;

use nuclei::io::{AsyncReadExt, AsyncWriteExt, Interest};
use nuclei::net::{TcpListener, TcpStream, UdpSocket, UnixStream};

fn within<F: core::future::Future>(f: F) -> F::Output {
    nuclei::block_on(async {
        nuclei::time::timeout(Duration::from_secs(30), f)
            .await
            .expect("nothing came within thirty seconds")
    })
}

async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (client, server)
}

/// `recv(2)` on `fd`, into `buf`.
fn raw_recv(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for `buf.len()` bytes.
    let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// `send(2)` on `fd`, of `buf`.
fn raw_send(fd: i32, buf: &[u8]) -> io::Result<usize> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = 0;
    // SAFETY: `buf` is valid for `buf.len()` bytes.
    let n = unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), flags) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// `len` bytes of `stream`: by a raw receive, or, when the handle holds
/// data it read ahead (`ResourceBusy`), by its own reads.
async fn raw_or_read(stream: &mut TcpStream, fd: i32, len: usize) -> Vec<u8> {
    let mut buf = [0u8; 16];
    let mut got = Vec::new();
    match stream
        .async_io(Interest::READABLE, || raw_recv(fd, &mut buf))
        .await
    {
        Ok(n) => got.extend_from_slice(&buf[..n]),
        Err(err) => assert_eq!(err.kind(), io::ErrorKind::ResourceBusy),
    }
    while got.len() < len {
        let n = stream.read(&mut buf).await.unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    got
}

#[test]
fn a_raw_receive_and_send_run_once_the_socket_may_be_ready() {
    within(async {
        let (mut client, server) = pair().await;
        let fd = server.as_raw_fd();
        let mut buf = [0u8; 16];
        let err = server
            .try_io(Interest::READABLE, || raw_recv(fd, &mut buf))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        client.write_all(b"raw").await.unwrap();
        client.flush().await.unwrap();
        let n = server
            .async_io(Interest::READABLE, || raw_recv(fd, &mut buf))
            .await
            .unwrap();
        assert_eq!(&buf[..n], b"raw");
        let n = server
            .async_io(Interest::WRITABLE, || raw_send(fd, b"back"))
            .await
            .unwrap();
        assert_eq!(n, 4);
        let mut back = [0u8; 4];
        client.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"back");
    });
}

#[test]
fn bytes_written_behind_go_out_before_a_raw_send() {
    within(async {
        let (mut client, mut server) = pair().await;
        let fd = server.as_raw_fd();
        server.write_all(b"first,").await.unwrap();
        server
            .async_io(Interest::WRITABLE, || raw_send(fd, b"second"))
            .await
            .unwrap();
        let mut got = [0u8; 12];
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"first,second");
    });
}

#[test]
fn data_read_ahead_is_read_before_a_raw_receive() {
    within(async {
        let (mut client, mut server) = pair().await;
        let fd = server.as_raw_fd();
        client.write_all(b"ahead").await.unwrap();
        client.flush().await.unwrap();
        // The handle reads ahead for its own reads.
        server.readable().await.unwrap();
        let mut buf = [0u8; 16];
        let err = server
            .async_io(Interest::READABLE, || raw_recv(fd, &mut buf))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ResourceBusy);
        let mut ahead = [0u8; 5];
        server.read_exact(&mut ahead).await.unwrap();
        assert_eq!(&ahead, b"ahead");
        // On a ring the handle went on receiving after its read, so what
        // comes next may be read ahead too: once, in order, either way.
        client.write_all(b"after").await.unwrap();
        client.flush().await.unwrap();
        assert_eq!(raw_or_read(&mut server, fd, 5).await, b"after");
        // And the handle's own reads take over again.
        client.write_all(b"own").await.unwrap();
        client.flush().await.unwrap();
        let mut own = [0u8; 3];
        server.read_exact(&mut own).await.unwrap();
        assert_eq!(&own, b"own");
    });
}

/// A read dropped while its receive is out (on a ring) leaves it out: a
/// raw receive after it stops it first, and every byte arrives once, in
/// order, through one or the other.
#[test]
fn a_receive_left_out_by_a_dropped_read_is_stopped_first() {
    within(async {
        let (mut client, mut server) = pair().await;
        let fd = server.as_raw_fd();
        let mut one = [0u8; 1];
        let dropped = nuclei::time::timeout(Duration::from_millis(20), server.read(&mut one)).await;
        assert!(dropped.is_err(), "nothing was written yet");
        client.write_all(b"xyz").await.unwrap();
        client.flush().await.unwrap();
        assert_eq!(raw_or_read(&mut server, fd, 3).await, b"xyz");
    });
}

/// A raw receive that waits while the dropped read's receive is out stops
/// it, and then takes the bytes itself. The write comes well after the
/// wait began (an ordering of two events, not a poll), so the stop is in.
#[test]
fn a_waiting_raw_receive_takes_the_bytes_a_dropped_read_left_its_receive_for() {
    within(async {
        let (mut client, mut server) = pair().await;
        let fd = server.as_raw_fd();
        let mut one = [0u8; 1];
        let dropped = nuclei::time::timeout(Duration::from_millis(20), server.read(&mut one)).await;
        assert!(dropped.is_err(), "nothing was written yet");
        let mut buf = [0u8; 16];
        let (raw, ()) = nuclei::join!(
            server.async_io(Interest::READABLE, || raw_recv(fd, &mut buf)),
            async {
                nuclei::time::sleep(Duration::from_millis(100)).await;
                client.write_all(b"mine").await.unwrap();
                client.flush().await.unwrap();
            }
        );
        let n = raw.expect("the raw receive took the bytes");
        assert_eq!(&buf[..n], b"mine");
    });
}

#[test]
fn a_datagram_socket_takes_raw_calls_too() {
    within(async {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        a.connect(b.local_addr().unwrap()).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        let (fa, fb) = (a.as_raw_fd(), b.as_raw_fd());
        a.async_io(Interest::WRITABLE, || raw_send(fa, b"dgram"))
            .await
            .unwrap();
        let mut buf = [0u8; 16];
        let n = b
            .async_io(Interest::READABLE, || raw_recv(fb, &mut buf))
            .await
            .unwrap();
        assert_eq!(&buf[..n], b"dgram");
    });
}

#[test]
fn a_unix_stream_takes_raw_calls_too() {
    within(async {
        let (a, mut b) = UnixStream::pair().unwrap();
        let fd = a.as_raw_fd();
        b.write_all(b"unix").await.unwrap();
        b.flush().await.unwrap();
        let mut buf = [0u8; 8];
        let n = a
            .async_io(Interest::READABLE, || raw_recv(fd, &mut buf))
            .await
            .unwrap();
        assert_eq!(&buf[..n], b"unix");
    });
}
