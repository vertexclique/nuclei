//! Bytes a stream's writes took into its write-behind reach the peer though
//! the thread whose `block_on` wrote them returned and no task polls that
//! handle's writes again: on a worker, or with none at the thread's next
//! `block_on`.

use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;
use std::time::Duration;

use nuclei::net::{TcpListener, TcpSocket, TcpStream};
use nuclei::runtime::Builder;

fn byte(at: usize) -> u8 {
    (at.wrapping_mul(31) ^ (at >> 8)) as u8
}

/// A connected pair whose client has a small send buffer, so its writes
/// soon go behind.
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_send_buffer_size(4096).unwrap();
    let client = socket
        .connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (client, server)
}

/// Writes until a write would wait, the write-behind full; returns the bytes
/// the writes took.
async fn fill(conn: &TcpStream) -> usize {
    let data: Vec<u8> = (0..4 << 20).map(byte).collect();
    let mut sent = 0;
    loop {
        let mut send = pin!(conn.send(&data[sent..]));
        match poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx))).await {
            Poll::Ready(n) => sent += n.unwrap(),
            Poll::Pending => return sent,
        }
    }
}

async fn recv_all(conn: &TcpStream, total: usize) {
    let mut buf = vec![0u8; 64 << 10];
    let mut at = 0;
    while at < total {
        let n = nuclei::time::timeout(Duration::from_secs(10), conn.recv(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("{} of {total} bytes never came", total - at))
            .unwrap();
        assert_ne!(n, 0, "the stream ended at {at} of {total}");
        assert!(
            buf[..n].iter().enumerate().all(|(i, b)| *b == byte(at + i)),
            "bytes differ from {at}"
        );
        at += n;
    }
}

#[test]
fn bytes_behind_reach_the_peer_from_a_worker() {
    let (client, server, sent) = nuclei::block_on(async {
        let (client, server) = pair().await;
        let sent = fill(&client).await;
        (client, server, sent)
    });
    std::thread::spawn(move || nuclei::block_on(recv_all(&server, sent)))
        .join()
        .unwrap();
    drop(client);
}

#[test]
fn bytes_behind_reach_the_peer_at_the_next_block_on_with_no_worker() {
    let rt = Builder::new().worker_threads(0).build().unwrap();
    let (client, server, sent) = rt.block_on(async {
        let (client, server) = pair().await;
        let sent = fill(&client).await;
        (client, server, sent)
    });
    rt.block_on(recv_all(&server, sent));
    drop(client);
}
