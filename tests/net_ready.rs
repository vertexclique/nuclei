//! The sockets' readiness calls and the reads and writes that do not wait:
//! ready means the handle's own buffers, so a `try_` call after a readiness
//! call answers at once.

use std::io::ErrorKind;
use std::time::Duration;

use nuclei::io::{Interest, Ready};
use nuclei::net::{TcpListener, TcpStream, UdpSocket};

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

#[test]
fn a_stream_reads_what_came_without_waiting() {
    within(async {
        let (client, server) = pair().await;
        let mut buf = [0u8; 16];
        assert_eq!(
            server.try_read(&mut buf).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        client.send(b"ready").await.unwrap();
        server.readable().await.unwrap();
        assert_eq!(server.try_read(&mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], b"ready");
        assert_eq!(
            server.try_read(&mut buf).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );

        client.send(b"vec").await.unwrap();
        server.readable().await.unwrap();
        let (mut a, mut b) = ([0u8; 2], [0u8; 4]);
        let mut bufs = [
            std::io::IoSliceMut::new(&mut a),
            std::io::IoSliceMut::new(&mut b),
        ];
        assert_eq!(server.try_read_vectored(&mut bufs).unwrap(), 3);
        assert_eq!((&a, &b[..1]), (b"ve", &b"c"[..]));

        // The end of the stream: readable, and read closed.
        drop(client);
        let ready = server.ready(Interest::READABLE).await.unwrap();
        assert!(ready.is_readable() && ready.is_read_closed());
        assert_eq!(server.try_read(&mut buf).unwrap(), 0);
    });
}

#[test]
fn a_stream_is_writable_at_once_and_waits_once_its_write_behind_is_full() {
    within(async {
        let (client, server) = pair().await;
        let ready = client
            .ready(Interest::READABLE | Interest::WRITABLE)
            .await
            .unwrap();
        assert_eq!(ready, Ready::WRITABLE, "nothing to read yet");
        client.writable().await.unwrap();
        let chunk = [7u8; 64 * 1024];
        let mut taken = 0usize;
        // The peer reads nothing: the kernel's buffers fill, then the
        // write-behind.
        let full = loop {
            match client.try_write(&chunk) {
                Ok(n) => taken += n,
                Err(err) => break err,
            }
            assert!(taken < 1 << 30, "the write-behind never filled");
        };
        assert_eq!(full.kind(), ErrorKind::WouldBlock);
        let waiting = nuclei::spawn(async move {
            client.writable().await.unwrap();
            client
        });
        // The peer drains it all; then the writer is writable again.
        let mut got = 0usize;
        let mut buf = vec![0u8; 256 * 1024];
        while got < taken {
            got += server.recv(&mut buf).await.unwrap();
        }
        let client = waiting.await;
        assert!(client.try_write(b"more").unwrap() > 0);
    });
}

#[test]
fn a_datagram_is_received_whole_after_readable() {
    within(async {
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let to = b.local_addr().unwrap();
        // Readiness first, before any receive said how much it takes: the
        // datagram read ahead must still be whole.
        a.send_to(&[5u8; 1200], to).await.unwrap();
        b.readable().await.unwrap();
        let mut buf = [0u8; 2048];
        let (n, _) = b.try_recv_from(&mut buf).unwrap();
        assert_eq!(n, 1200, "the datagram read ahead was cut");
        assert!(buf[..n].iter().all(|&x| x == 5));
        assert_eq!(
            b.try_recv_from(&mut buf).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        let datagram = [3u8; 1200];
        assert_eq!(a.try_send_to(&datagram, to).unwrap(), 1200);
        b.readable().await.unwrap();
        // A peek leaves it, its sender and all of it.
        let (n, from) = b.try_peek_from(&mut buf).unwrap();
        assert_eq!((n, from), (1200, a.local_addr().unwrap()));
        let (n, from) = b.try_recv_from(&mut buf).unwrap();
        assert_eq!(
            (n, from),
            (1200, a.local_addr().unwrap()),
            "read ahead whole"
        );
        assert!(buf[..n].iter().all(|&x| x == 3));
        assert_eq!(
            b.try_recv(&mut buf).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );

        a.connect(to).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        a.writable().await.unwrap();
        assert_eq!(a.try_send(b"one").unwrap(), 3);
        b.readable().await.unwrap();
        assert_eq!(b.try_peek(&mut buf).unwrap(), 3);
        assert_eq!(b.try_recv(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"one");
    });
}

#[cfg(unix)]
#[test]
fn a_unix_datagram_socket_receives_without_waiting() {
    within(async {
        let (a, b) = nuclei::net::UnixDatagram::pair().unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(
            b.try_recv(&mut buf).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        a.writable().await.unwrap();
        assert_eq!(a.try_send(b"pair").unwrap(), 4);
        b.readable().await.unwrap();
        let (n, from) = b.try_recv_from(&mut buf).unwrap();
        assert_eq!(n, 4);
        assert!(from.is_unnamed());
        assert_eq!(&buf[..4], b"pair");
    });
}
