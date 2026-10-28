//! `nuclei::net` beyond the builders: names looked up off the shard, the
//! poll forms of accept and peek, a datagram's sender peeked, receives
//! into a `ReadBuf`, and on unix abstract addresses, unbound datagram
//! sockets and a peer's credentials.

use core::future::poll_fn;
use std::time::Duration;

use nuclei::io::ReadBuf;
use nuclei::net::{TcpListener, TcpStream, UdpSocket, lookup_host};

#[test]
fn names_resolve_off_the_shard_and_numbers_at_once() {
    nuclei::block_on(async {
        let listener = TcpListener::bind("localhost:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
        let stream = TcpStream::connect(("localhost", port)).await.unwrap();
        let (_server, peer) = listener.accept().await.unwrap();
        assert_eq!(peer, stream.local_addr().unwrap());
        let addrs: Vec<_> = lookup_host(("127.0.0.1", 9)).await.unwrap().collect();
        assert_eq!(addrs, ["127.0.0.1:9".parse().unwrap()]);
        let err = TcpListener::bind("no.such.host.invalid:0")
            .await
            .unwrap_err();
        assert_ne!(err.kind(), std::io::ErrorKind::AddrInUse);
    });
}

#[test]
fn poll_accept_and_poll_peek_answer_as_their_futures() {
    nuclei::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = poll_fn(|cx| listener.poll_accept(cx)).await.unwrap();
        client.send(b"peeked").await.unwrap();
        let mut storage = [0u8; 16];
        let mut buf = ReadBuf::new(&mut storage);
        let mut n = 0;
        // A peek may see part of what was sent: look until all of it came.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while n < 6 {
            assert!(std::time::Instant::now() < deadline, "the bytes never came");
            buf.clear();
            n = poll_fn(|cx| server.poll_peek(cx, &mut buf)).await.unwrap();
        }
        assert_eq!(buf.filled(), b"peeked");
        // Peeked, not taken.
        let mut got = [0u8; 6];
        assert_eq!(server.recv(&mut got).await.unwrap(), 6);
        assert_eq!(&got, b"peeked");
    });
}

#[test]
fn a_datagrams_sender_is_peeked_and_it_stays_to_be_received() {
    nuclei::block_on(async {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b_addr = b.local_addr().unwrap();
        a.send_to(b"hello", b_addr).await.unwrap();
        assert_eq!(b.peek_sender().await.unwrap(), a.local_addr().unwrap());
        assert_eq!(
            poll_fn(|cx| b.poll_peek_sender(cx)).await.unwrap(),
            a.local_addr().unwrap()
        );
        let mut storage = [0u8; 3];
        let mut buf = ReadBuf::new(&mut storage);
        let (n, from) = b.recv_buf_from(&mut buf).await.unwrap();
        assert_eq!((n, from), (3, a.local_addr().unwrap()), "cut to the buffer");
        assert_eq!(buf.filled(), b"hel");

        a.connect(b_addr).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        a.send(b"again").await.unwrap();
        let mut storage = [0u8; 8];
        let mut buf = ReadBuf::new(&mut storage);
        assert_eq!(b.recv_buf(&mut buf).await.unwrap(), 5);
        assert_eq!(buf.filled(), b"again");
    });
}

#[cfg(unix)]
mod unix {
    use std::os::unix::net::SocketAddr;

    use nuclei::net::{UnixDatagram, UnixListener, UnixStream};

    fn path(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("nuclei-net-api-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_listener_and_a_stream_meet_by_std_address() {
        let path = path("by-addr");
        nuclei::block_on(async {
            let addr = SocketAddr::from_pathname(&path).unwrap();
            let listener = UnixListener::bind_addr(&addr).unwrap();
            let client = UnixStream::connect_addr(&addr).await.unwrap();
            let (_server, _) = listener.accept().await.unwrap();
            assert!(client.peer_addr().unwrap().as_pathname() == Some(path.as_path()));
        });
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn abstract_addresses_connect_and_send() {
        #[cfg(target_os = "android")]
        use std::os::android::net::SocketAddrExt;
        #[cfg(target_os = "linux")]
        use std::os::linux::net::SocketAddrExt;
        nuclei::block_on(async {
            let name = format!("nuclei-net-api-{}", std::process::id());
            let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
            let listener = UnixListener::bind_addr(&addr).unwrap();
            let client = UnixStream::connect_addr(&addr).await.unwrap();
            let (server, _) = listener.accept().await.unwrap();
            client.send(b"abs").await.unwrap();
            let mut buf = [0u8; 3];
            let mut got = 0;
            while got < 3 {
                got += server.recv(&mut buf[got..]).await.unwrap();
            }
            assert_eq!(&buf, b"abs");

            let dgram_name = format!("nuclei-net-api-dgram-{}", std::process::id());
            let dgram_addr = SocketAddr::from_abstract_name(dgram_name.as_bytes()).unwrap();
            let receiver = UnixDatagram::bind_addr(&dgram_addr).unwrap();
            let sender = UnixDatagram::unbound().unwrap();
            sender.connect_addr(&dgram_addr).unwrap();
            sender.send(b"dg").await.unwrap();
            let mut buf = [0u8; 2];
            assert_eq!(receiver.recv(&mut buf).await.unwrap(), 2);
            assert_eq!(&buf, b"dg");
        });
    }

    #[test]
    fn an_unbound_datagram_socket_sends_to_a_path() {
        let path = path("unbound");
        nuclei::block_on(async {
            let receiver = UnixDatagram::bind(&path).unwrap();
            let sender = UnixDatagram::unbound().unwrap();
            assert!(sender.local_addr().unwrap().is_unnamed());
            sender.send_to(b"x", &path).await.unwrap();
            let mut buf = [0u8; 1];
            let (n, from) = receiver.recv_from(&mut buf).await.unwrap();
            assert_eq!((n, &buf), (1, b"x"));
            assert!(from.is_unnamed());
        });
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_streams_peer_credentials_are_this_process() {
        nuclei::block_on(async {
            let (a, _b) = UnixStream::pair().unwrap();
            let cred = a.peer_cred().unwrap();
            // SAFETY: plain calls with no arguments.
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            assert_eq!((cred.uid(), cred.gid()), (uid, gid));
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
            assert_eq!(cred.pid(), Some(std::process::id() as i32));
        });
    }

    #[test]
    fn an_unnamed_address_names_no_listener() {
        let unnamed = UnixDatagram::unbound().unwrap().local_addr().unwrap();
        let err = nuclei::block_on(UnixStream::connect_addr(&unnamed)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
