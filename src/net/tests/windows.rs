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
        let sent = fill_behind(&handle).await;
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
