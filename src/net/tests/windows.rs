//! The descriptor rules where a socket is a handle: a value the system
//! hands to the next socket once the old one closes.

use std::os::windows::io::AsRawSocket;

use super::*;

/// A handle whose socket closed while a receive was registered: the next
/// socket takes the same handle value, and only its own bytes reach it.
/// Alone in its process, so the next sockets are this test's.
#[test]
fn a_handle_value_taken_by_the_next_socket_never_sees_the_old_registration() {
    if !alone(
        "net::tests::windows::a_handle_value_taken_by_the_next_socket_never_sees_the_old_registration",
    ) {
        return;
    }
    let rt = runtime();
    rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = within(Handle::<TcpStream>::connect(addr)).await.unwrap();
        let (old_peer, _) = listener.accept().unwrap();
        let mut buf = [0u8; 8];
        {
            let mut waiting = Box::pin(handle.recv(&mut buf));
            let polled = poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx))).await;
            assert!(polled.is_pending());
        }
        let value = handle.as_raw_socket();
        drop(handle.into_inner());
        // New connections until one of their sockets has the value; the
        // others stay open, so they do not hand theirs back.
        let mut others = Vec::new();
        let (taker, taker_peer) = loop {
            assert!(others.len() < 64, "no new socket took the freed value");
            let client = TcpStream::connect(addr).unwrap();
            let (server, _) = listener.accept().unwrap();
            if client.as_raw_socket() == value {
                break (client, server);
            }
            if server.as_raw_socket() == value {
                break (server, client);
            }
            others.push((client, server));
        };
        let taker = Handle::new(taker).unwrap();
        (&old_peer).write_all(b"old").unwrap();
        (&taker_peer).write_all(b"mine").unwrap();
        let n = within(taker.recv(&mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"mine");
        drop(others);
    });
}

/// Writes until the socket takes nothing more for a while (Windows' TCP
/// grows its buffers, so a first write that goes behind is not enough):
/// what is left behind then cannot go out until the peer reads. Returns
/// every byte the handle took, in order.
async fn fill_until_stuck(h: &Handle<TcpStream>) -> Vec<u8> {
    let mut sent = Vec::new();
    let mut seq = 0usize;
    loop {
        let chunk: Vec<u8> = (0..64 * 1024)
            .map(|i| ((seq * 64 * 1024 + i) % 251) as u8)
            .collect();
        seq += 1;
        match crate::time::timeout(Duration::from_millis(100), h.send(&chunk)).await {
            Ok(n) => sent.extend_from_slice(&chunk[..n.unwrap()]),
            // A send that waits took none of its bytes.
            Err(_) => return sent,
        }
    }
}

/// A handle dropped with bytes still to send keeps its socket open until
/// they are out: the next socket gets another value, the bytes reach the
/// old peer whole, and none reaches the new socket.
#[test]
fn a_dropped_handle_keeps_its_value_until_its_bytes_are_out() {
    if !alone("net::tests::windows::a_dropped_handle_keeps_its_value_until_its_bytes_are_out") {
        return;
    }
    let rt = runtime();
    let (mine, peer) = stream_pair();
    let (sent, value) = rt.block_on(async {
        let handle = Handle::new(mine).unwrap();
        let sent = fill_until_stuck(&handle).await;
        assert!(handle.source().has_unsent(), "nothing was left to send");
        let value = handle.as_raw_socket();
        drop(handle);
        (sent, value)
    });
    let (taker, taker_peer) = stream_pair();
    assert_ne!(
        taker.as_raw_socket(),
        value,
        "the value was handed on while bytes were still to send"
    );
    let got = std::thread::spawn(move || read_to_end_blocking(peer))
        .join()
        .unwrap();
    assert!(
        got == sent,
        "{} of {} bytes reached the old peer",
        got.len(),
        sent.len()
    );
    drop(taker);
    taker_peer
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut buf = [0u8; 1];
    assert!(
        matches!((&taker_peer).read(&mut buf), Ok(0)),
        "bytes leaked into the new socket"
    );
}

/// A stream whose thread's `block_on` returned (and then the thread) while
/// its send was still out, stuck until the peer reads, goes on from
/// another thread: the port it was on is kept while the send ends, the
/// stream registers again once it did, and the peer gets every byte once,
/// in order.
#[test]
fn a_stream_left_with_a_send_out_goes_on_from_another_thread() {
    let (mine, peer) = stream_pair();
    let (handle, mut sent) = std::thread::spawn(move || {
        crate::block_on(async {
            let handle = Handle::new(mine).unwrap();
            let sent = fill_until_stuck(&handle).await;
            assert!(handle.source().has_unsent(), "nothing was left to send");
            (handle, sent)
        })
    })
    .join()
    .unwrap();
    let reader = std::thread::spawn(move || read_to_end_blocking(peer));
    let tail = b"the rest, from another thread";
    runtime().block_on(async {
        let mut at = 0;
        while at < tail.len() {
            at += within(handle.send(&tail[at..])).await.unwrap();
        }
    });
    drop(handle);
    sent.extend_from_slice(tail);
    let got = reader.join().unwrap();
    assert!(
        got == sent,
        "{} of {} bytes reached the peer, or not in order",
        got.len(),
        sent.len()
    );
}

/// A stream nuclei connects binds its port as std's connect binds it: one that connections to
/// other destinations may share (`SO_REUSE_UNICASTPORT`), so a client that makes many does not
/// run out of ports while they wait out TIME_WAIT. Each of two listeners takes a connection from
/// the same client, and both work.
#[test]
fn a_connect_binds_a_port_other_destinations_may_share() {
    use crate::syscore::windows::{SO_REUSE_UNICASTPORT, SOCKET, SOL_SOCKET, getsockopt};
    let rt = runtime();
    rt.block_on(async {
        for _ in 0..2 {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let stream = within(Handle::<TcpStream>::connect(listener.local_addr().unwrap()))
                .await
                .unwrap();
            let (mut on, mut len) = (0i32, size_of::<i32>() as i32);
            // SAFETY: the option is an int, written into our own.
            let ret = unsafe {
                getsockopt(
                    stream.as_raw_socket() as SOCKET,
                    SOL_SOCKET,
                    SO_REUSE_UNICASTPORT,
                    core::ptr::from_mut(&mut on).cast(),
                    &mut len,
                )
            };
            assert_eq!(ret, 0, "{}", io::Error::last_os_error());
            assert_eq!(on, 1, "the connect's port is one of its own");
            let (peer, _) = listener.accept().unwrap();
            (&peer).write_all(b"ok").unwrap();
            let mut buf = [0u8; 2];
            let mut got = 0;
            while got < 2 {
                got += within(stream.recv(&mut buf[got..])).await.unwrap();
            }
            assert_eq!(&buf, b"ok");
        }
    });
}

/// A socket nuclei takes over answers `WouldBlock` where it would block, as a unix descriptor
/// does: std's own calls through `get_ref` never block the shard.
#[test]
fn an_adopted_socket_does_not_block() {
    let rt = runtime();
    rt.block_on(async {
        let udp = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
        let err = udp.get_ref().recv_from(&mut [0u8; 16]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").await.unwrap();
        let err = listener.get_ref().accept().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    });
}

/// Each datagram's send answers what the kernel says of that datagram: a refused one (a
/// broadcast with `SO_BROADCAST` off) fails its own call, and the calls around it succeed.
#[test]
fn a_datagram_send_answers_its_own_verdict() {
    let rt = runtime();
    rt.block_on(async {
        let a = Handle::<UdpSocket>::bind("0.0.0.0:0").await.unwrap();
        let b = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
        let to = b.local_addr().unwrap();
        let port = a.local_addr().unwrap().port();
        assert_eq!(within(a.send_to(b"one", to)).await.unwrap(), 3);
        let refused = within(a.send_to(b"all", ("255.255.255.255", port))).await;
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(within(a.send_to(b"two", to)).await.unwrap(), 3);
        let mut buf = [0u8; 8];
        for want in [&b"one"[..], b"two"] {
            let (n, _) = within(b.recv_from(&mut buf)).await.unwrap();
            assert_eq!(&buf[..n], want);
        }
    });
}

/// A burst of datagrams through a socket with no send buffer: each send from the task either
/// goes or, finding no room, waits for the port's poll of the socket and tries again; every one
/// arrives, whole and in order.
#[test]
fn a_burst_of_datagram_sends_waits_for_room_and_loses_none() {
    const COUNT: usize = 400;
    const LEN: usize = 1200;
    let rt = runtime();
    rt.block_on(async {
        let a = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
        let b = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
        let raw = |s: &Handle<UdpSocket>| s.get_ref().as_raw_socket();
        crate::net::opt::set_size(
            raw(&a),
            crate::net::opt::SOL_SOCKET,
            crate::net::opt::SO_SNDBUF,
            0,
        )
        .unwrap();
        crate::net::opt::set_size(
            raw(&b),
            crate::net::opt::SOL_SOCKET,
            crate::net::opt::SO_RCVBUF,
            8 << 20,
        )
        .unwrap();
        let to = b.local_addr().unwrap();
        let datagram = |i: usize| -> Vec<u8> { (0..LEN).map(|j| (i * 7 + j) as u8).collect() };
        for i in 0..COUNT {
            assert_eq!(within(a.send_to(&datagram(i), to)).await.unwrap(), LEN);
        }
        let mut buf = vec![0u8; 2 * LEN];
        for i in 0..COUNT {
            let (n, _) = within(b.recv_from(&mut buf)).await.unwrap();
            assert!(buf[..n] == datagram(i)[..], "datagram {i} differs");
        }
    });
}

/// A stream whose send, never cancelled, waits for a peer that reads only after it heard from
/// us: `block_on` returns with that send out, and the source is let go with it. The next read
/// is served all the same, on the thread's own shard again (whose port the handle is bound to)
/// or on a runtime's worker (another port, which the handle cannot move to while its send is
/// out: it stays on its old port, whose keeper drains it, until the send ended), and every byte
/// of the send reaches the peer.
#[test]
fn a_moved_stream_reads_while_its_send_waits_for_the_peer() {
    for on_worker in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let to = listener.local_addr().unwrap();
        let small = |raw| {
            crate::net::opt::set_size(
                raw,
                crate::net::opt::SOL_SOCKET,
                crate::net::opt::SO_RCVBUF,
                1024,
            )
            .unwrap();
        };
        small(listener.as_raw_socket());
        let sent: Vec<u8> = (0..48 * 1024).map(|i| (i % 251) as u8).collect();
        let (heard_tx, heard_rx) = std::sync::mpsc::channel::<()>();
        let peer = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            peer.write_all(b"ping").unwrap();
            // It reads nothing until we read its ping.
            heard_rx.recv().unwrap();
            let mut got = Vec::new();
            peer.read_to_end(&mut got).unwrap();
            got
        });
        let mut stream = crate::block_on(within(Handle::<TcpStream>::connect(to))).unwrap();
        crate::net::opt::set_size(
            stream.as_raw_socket(),
            crate::net::opt::SOL_SOCKET,
            crate::net::opt::SO_SNDBUF,
            1024,
        )
        .unwrap();
        // The bytes are taken, most of them behind, in a send the peer does not take yet.
        crate::block_on(within(write_all_unflushed(&mut stream, &sent)));
        let read = |mut stream: Handle<TcpStream>| async move {
            let mut ping = [0u8; 4];
            within(read_exact(&mut stream, &mut ping)).await.unwrap();
            assert_eq!(&ping, b"ping");
            stream
        };
        let mut stream = if on_worker {
            // On a worker: another shard, another port.
            let rt = runtime();
            rt.block_on(rt.spawn(read(stream)))
        } else {
            crate::block_on(read(stream))
        };
        heard_tx.send(()).unwrap();
        crate::block_on(within(close(&mut stream))).unwrap();
        drop(stream);
        let got = peer.join().unwrap();
        assert!(
            got == sent,
            "{} of {} bytes reached the peer, or not in order",
            got.len(),
            sent.len()
        );
    }
}

/// Writes all of `bytes`, waiting for none of them to leave the write-behind.
async fn write_all_unflushed<W: AsyncWrite + Unpin>(w: &mut W, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let n = poll_fn(|cx| Pin::new(&mut *w).poll_write(cx, bytes))
            .await
            .unwrap();
        bytes = &bytes[n..];
    }
}
