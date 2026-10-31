//! `UdpSocketBuilder`: its options read back as set before it binds, two sockets share a port
//! with `SO_REUSEPORT` and only with it, its bind answers as std's, and the socket it makes
//! sends and receives; and `UdpSocket`'s `poll_send_to` and `poll_recv_from` answer as their
//! futures do, a datagram cut to the buffer included.

use core::future::poll_fn;
use std::io::ErrorKind;
use std::net::SocketAddr;

use nuclei::io::ReadBuf;
use nuclei::net::{UdpSocket, UdpSocketBuilder};

fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

#[test]
fn a_udp_builders_options_read_back_as_set() {
    let builder = UdpSocketBuilder::new_v4().unwrap();
    for on in [true, false] {
        builder.set_reuseaddr(on).unwrap();
        assert_eq!(builder.reuseaddr().unwrap(), on);
        #[cfg(unix)]
        {
            builder.set_reuseport(on).unwrap();
            assert_eq!(builder.reuseport().unwrap(), on);
        }
    }
    builder.set_send_buffer_size(64 * 1024).unwrap();
    assert!(builder.send_buffer_size().unwrap() >= 64 * 1024);
    builder.set_recv_buffer_size(64 * 1024).unwrap();
    assert!(builder.recv_buffer_size().unwrap() >= 64 * 1024);
    #[cfg(unix)]
    {
        builder.set_tos(0x10).unwrap();
        assert_eq!(builder.tos().unwrap(), 0x10);
    }
    assert!(builder.take_error().unwrap().is_none());
    // Windows names no address of a socket not bound yet.
    #[cfg(unix)]
    assert_eq!(builder.local_addr().unwrap().port(), 0);
    #[cfg(windows)]
    assert_eq!(
        builder.local_addr().unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    // An IPv4 socket has no IPv6 option on unix; Windows takes it.
    #[cfg(unix)]
    assert!(builder.set_only_v6(true).is_err());
    // IPv6 may be off on the host: its options are checked only if it is on.
    if let Ok(v6) = UdpSocketBuilder::new_v6() {
        for only in [true, false] {
            v6.set_only_v6(only).unwrap();
            assert_eq!(v6.only_v6().unwrap(), only);
        }
        #[cfg(unix)]
        {
            v6.set_tclass_v6(0x20).unwrap();
            assert_eq!(v6.tclass_v6().unwrap(), 0x20);
        }
    }
}

/// `SO_BINDTODEVICE` needs no privilege since Linux 5.7 for a socket not bound to a device
/// yet; on an older kernel or a host that refuses it, the call's error is all there is to check.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_udp_builder_binds_to_a_device() {
    let builder = UdpSocketBuilder::new_v4().unwrap();
    assert_eq!(builder.device().unwrap(), None);
    match builder.bind_device(Some(b"lo")) {
        Ok(()) => assert_eq!(builder.device().unwrap().as_deref(), Some(&b"lo"[..])),
        Err(err) => assert_eq!(err.kind(), ErrorKind::PermissionDenied),
    }
}

#[test]
fn the_socket_a_builder_binds_sends_and_receives() {
    nuclei::block_on(async {
        let builder = UdpSocketBuilder::new_v4().unwrap();
        builder.set_recv_buffer_size(256 * 1024).unwrap();
        let a = builder.bind(loopback()).unwrap();
        let addr = a.local_addr().unwrap();
        assert_ne!(addr.port(), 0);
        let b = UdpSocket::bind(loopback()).await.unwrap();
        b.send_to(b"ping", addr).await.unwrap();
        let mut buf = [0u8; 8];
        let (n, from) = a.recv_from(&mut buf).await.unwrap();
        assert_eq!((&buf[..n], from), (&b"ping"[..], b.local_addr().unwrap()));
        a.send_to(b"pong", from).await.unwrap();
        let (n, back) = b.recv_from(&mut buf).await.unwrap();
        assert_eq!((&buf[..n], back), (&b"pong"[..], addr));
    });
}

#[test]
fn a_builders_bind_answers_as_std() {
    let held = std::net::UdpSocket::bind(loopback()).unwrap();
    let taken = held.local_addr().unwrap();
    let want = std::net::UdpSocket::bind(taken).unwrap_err();
    let err = UdpSocketBuilder::new_v4().unwrap().bind(taken).unwrap_err();
    assert_eq!(
        (err.kind(), err.raw_os_error()),
        (want.kind(), want.raw_os_error())
    );
    // An address of the other family is refused as std's is.
    let err = UdpSocketBuilder::new_v4()
        .unwrap()
        .bind("[::1]:0".parse().unwrap())
        .unwrap_err();
    assert_ne!(err.kind(), ErrorKind::AddrInUse, "{err}");
}

#[cfg(unix)]
#[test]
fn two_sockets_share_a_port_with_reuseport_and_only_with_it() {
    let first = UdpSocketBuilder::new_v4().unwrap();
    first.set_reuseport(true).unwrap();
    let first = first.bind(loopback()).unwrap();
    let addr = first.local_addr().unwrap();
    let second = UdpSocketBuilder::new_v4().unwrap();
    second.set_reuseport(true).unwrap();
    let second = second.bind(addr).unwrap();
    assert_eq!(second.local_addr().unwrap(), addr);
    let err = UdpSocketBuilder::new_v4().unwrap().bind(addr).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AddrInUse, "{err}");
}

#[test]
fn poll_send_to_and_poll_recv_from_answer_as_their_futures() {
    nuclei::block_on(async {
        let a = UdpSocket::bind(loopback()).await.unwrap();
        let b = UdpSocket::bind(loopback()).await.unwrap();
        let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        let sent = poll_fn(|cx| a.poll_send_to(cx, b"hello", b_addr))
            .await
            .unwrap();
        assert_eq!(sent, 5);
        let mut storage = [0u8; 16];
        let mut buf = ReadBuf::new(&mut storage);
        let from = poll_fn(|cx| b.poll_recv_from(cx, &mut buf)).await.unwrap();
        assert_eq!((buf.filled(), from), (&b"hello"[..], a_addr));
        // What does not fit is cut, as the kernel cuts it; the buffer keeps what it held.
        poll_fn(|cx| a.poll_send_to(cx, b"world!", b_addr))
            .await
            .unwrap();
        let mut storage = [0u8; 8];
        let mut buf = ReadBuf::new(&mut storage);
        buf.put_slice(b"ab");
        let from = poll_fn(|cx| b.poll_recv_from(cx, &mut buf)).await.unwrap();
        assert_eq!((buf.filled(), from), (&b"abworld!"[..], a_addr));
        let mut storage = [0u8; 4];
        let mut buf = ReadBuf::new(&mut storage);
        poll_fn(|cx| a.poll_send_to(cx, b"longer", b_addr))
            .await
            .unwrap();
        poll_fn(|cx| b.poll_recv_from(cx, &mut buf)).await.unwrap();
        assert_eq!(buf.filled(), b"long");
    });
}
