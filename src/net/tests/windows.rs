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
