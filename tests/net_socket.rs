//! `nuclei::net`'s socket builders: `TcpSocket`'s options read back as
//! set, its bind, listen and connect, a socket taken from std, a stream's
//! linger, and `UnixSocket`'s three ends.

use std::time::Duration;

use nuclei::net::{TcpListener, TcpSocket, TcpStream};

/// A listener and a connected pair on loopback.
async fn pair() -> (TcpListener, TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (listener, client, server)
}

#[test]
fn tcp_socket_options_read_back_as_set() {
    let socket = TcpSocket::new_v4().unwrap();
    for on in [true, false] {
        socket.set_keepalive(on).unwrap();
        assert_eq!(socket.keepalive().unwrap(), on);
        socket.set_reuseaddr(on).unwrap();
        assert_eq!(socket.reuseaddr().unwrap(), on);
        socket.set_nodelay(on).unwrap();
        assert_eq!(socket.nodelay().unwrap(), on);
        #[cfg(unix)]
        {
            socket.set_reuseport(on).unwrap();
            assert_eq!(socket.reuseport().unwrap(), on);
        }
    }
    socket.set_send_buffer_size(64 * 1024).unwrap();
    assert!(socket.send_buffer_size().unwrap() >= 64 * 1024);
    socket.set_recv_buffer_size(64 * 1024).unwrap();
    assert!(socket.recv_buffer_size().unwrap() >= 64 * 1024);
    socket.set_linger(Some(Duration::from_secs(3))).unwrap();
    assert_eq!(socket.linger().unwrap(), Some(Duration::from_secs(3)));
    socket.set_zero_linger().unwrap();
    assert_eq!(socket.linger().unwrap(), Some(Duration::ZERO));
    socket.set_linger(None).unwrap();
    assert_eq!(socket.linger().unwrap(), None);
    #[cfg(unix)]
    {
        socket.set_tos(0x10).unwrap();
        assert_eq!(socket.tos().unwrap(), 0x10);
    }
    assert!(socket.take_error().unwrap().is_none());
    // IPv6 may be off on the host: its options are checked only if it is on.
    #[cfg(unix)]
    if let Ok(v6) = TcpSocket::new_v6() {
        v6.set_tclass_v6(0x20).unwrap();
        assert_eq!(v6.tclass_v6().unwrap(), 0x20);
    }
}

#[test]
fn a_tcp_socket_binds_listens_and_connects() {
    nuclei::block_on(async {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_reuseaddr(true).unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = socket.local_addr().unwrap();
        assert_ne!(addr.port(), 0);
        let listener = socket.listen(64).unwrap();
        assert_eq!(listener.local_addr().unwrap(), addr);

        let client = TcpSocket::new_v4().unwrap();
        client.set_nodelay(true).unwrap();
        let client = client.connect(addr).await.unwrap();
        assert!(client.nodelay().unwrap());
        let (server, peer) = listener.accept().await.unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
        client.send(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            got += server.recv(&mut buf[got..]).await.unwrap();
        }
        assert_eq!(&buf, b"ping");
    });
}

#[test]
fn a_connect_to_a_closed_port_is_refused() {
    nuclei::block_on(async {
        // On Linux a socket bound to the port and not listening holds it, so
        // no test running meanwhile is handed it; macOS drops a connect to
        // such a socket rather than refusing it, and hands out ports in turn,
        // so there (and on Windows) the port is bound and freed again.
        let (_held, addr) = if cfg!(any(target_os = "linux", target_os = "android")) {
            let held = TcpSocket::new_v4().unwrap();
            held.bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = held.local_addr().unwrap();
            (Some(held), addr)
        } else {
            let freed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            (None, freed.local_addr().unwrap())
        };
        let err = TcpSocket::new_v4()
            .unwrap()
            .connect(addr)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
    });
}

/// A socket made elsewhere, blocking, is set up as a nuclei one when it
/// connects.
#[cfg(unix)]
#[test]
fn a_socket_taken_from_std_connects() {
    use std::os::fd::{FromRawFd, IntoRawFd};
    nuclei::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fd = TcpSocket::new_v4().unwrap().into_raw_fd();
        // SAFETY: a socket of our own, handed over once.
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        std_stream.set_nonblocking(false).unwrap();
        let socket = TcpSocket::from_std_stream(std_stream);
        let stream = socket.connect(addr).await.unwrap();
        let (_server, peer) = listener.accept().await.unwrap();
        assert_eq!(peer, stream.local_addr().unwrap());
    });
}

#[test]
fn a_stream_lingers_as_set() {
    nuclei::block_on(async {
        let (_listener, client, server) = pair().await;
        assert_eq!(client.linger().unwrap(), None);
        client.set_linger(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(client.linger().unwrap(), Some(Duration::from_secs(1)));
        server.set_zero_linger().unwrap();
        assert_eq!(server.linger().unwrap(), Some(Duration::ZERO));
        client.set_linger(None).unwrap();
        assert_eq!(client.linger().unwrap(), None);
    });
}

/// `SO_BINDTODEVICE` needs no privilege since Linux 5.7 for a socket not
/// bound to a device yet; on an older kernel or a host that refuses it, the
/// call's error is all there is to check.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_tcp_socket_binds_to_a_device() {
    let socket = TcpSocket::new_v4().unwrap();
    assert_eq!(socket.device().unwrap(), None);
    match socket.bind_device(Some(b"lo")) {
        Ok(()) => assert_eq!(socket.device().unwrap().as_deref(), Some(&b"lo"[..])),
        Err(err) => assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied),
    }
}

#[cfg(unix)]
mod unix {
    use std::path::PathBuf;

    use nuclei::net::UnixSocket;

    /// A socket path of this test's own, removed before and after.
    struct Path(PathBuf);

    impl Path {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("nuclei-net-socket-{}-{name}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for Path {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_unix_socket_of_streams_listens_and_connects() {
        let path = Path::new("stream");
        nuclei::block_on(async {
            let socket = UnixSocket::new_stream().unwrap();
            socket.bind(&path.0).unwrap();
            let listener = socket.listen(16).unwrap();
            let client = UnixSocket::new_stream()
                .unwrap()
                .connect(&path.0)
                .await
                .unwrap();
            let (server, _) = listener.accept().await.unwrap();
            client.send(b"hey").await.unwrap();
            let mut buf = [0u8; 3];
            let mut got = 0;
            while got < 3 {
                got += server.recv(&mut buf[got..]).await.unwrap();
            }
            assert_eq!(&buf, b"hey");
        });
    }

    #[test]
    fn a_unix_socket_of_datagrams_sends_and_receives() {
        let (a_path, b_path) = (Path::new("dgram-a"), Path::new("dgram-b"));
        nuclei::block_on(async {
            let a = UnixSocket::new_datagram().unwrap();
            a.bind(&a_path.0).unwrap();
            let a = a.datagram().unwrap();
            let b = UnixSocket::new_datagram().unwrap();
            b.bind(&b_path.0).unwrap();
            let b = b.datagram().unwrap();
            a.send_to(b"one", &b_path.0).await.unwrap();
            let mut buf = [0u8; 8];
            let (n, from) = b.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"one");
            assert_eq!(from.as_pathname(), Some(a_path.0.as_path()));
        });
    }

    #[test]
    fn a_unix_socket_refuses_what_its_kind_cannot_do() {
        let err = UnixSocket::new_datagram().unwrap().listen(1).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = UnixSocket::new_stream().unwrap().datagram().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err =
            nuclei::block_on(UnixSocket::new_datagram().unwrap().connect("/nowhere")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
