//! A stream's owner sends straight from its task's buffer while the write side's last release
//! said it holds nothing (proofs/tla/sendsnow): its writes keep their order behind bytes the side
//! holds, and an owned write from another thread reaches the peer whole among them. Run on both
//! backends (`NUCLEI_BACKEND=epoll`).

use std::sync::Arc;
use std::time::Duration;

use nuclei::net::{TcpListener, TcpSocket, TcpStream};
use nuclei::runtime::Builder;

fn byte(at: usize) -> u8 {
    (at.wrapping_mul(31) ^ (at >> 8)) as u8
}

/// A connected pair whose client has a small send buffer, so its writes soon go behind.
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

/// Receives exactly `total` bytes, within a deadline for each.
async fn recv_exact(conn: &TcpStream, total: usize) -> Vec<u8> {
    let mut got = vec![0u8; total];
    let mut at = 0;
    while at < total {
        let n = nuclei::time::timeout(Duration::from_secs(30), conn.recv(&mut got[at..]))
            .await
            .unwrap_or_else(|_| panic!("{} of {total} bytes never came", total - at))
            .unwrap();
        assert_ne!(n, 0, "the stream ended at {at} of {total}");
        at += n;
    }
    got
}

/// One worker that never hands its shard to another thread, so a task's writes run as the
/// stream's owner.
fn runtime() -> nuclei::runtime::Runtime {
    Builder::new()
        .worker_threads(1)
        .handoff_after(None)
        .build()
        .unwrap()
}

async fn send_all(conn: &TcpStream, bytes: &[u8]) {
    let mut sent = 0;
    while sent < bytes.len() {
        sent += conn.send(&bytes[sent..]).await.unwrap();
    }
}

#[test]
fn an_owners_writes_keep_their_order_behind_bytes_the_side_holds() {
    const TOTAL: usize = 4 << 20;
    let rt = runtime();
    let (client, server) = rt.block_on(rt.spawn(pair()));
    // The peer reads on a thread of its own, so the socket takes bytes again while the owner's
    // task still runs and bytes it wrote wait behind.
    let reader = std::thread::spawn(move || nuclei::block_on(recv_exact(&server, TOTAL)));
    rt.block_on(rt.spawn(async move {
        let mut at = 0;
        while at < TOTAL {
            let end = (at + at % 700 + 1).min(TOTAL);
            let piece: Vec<u8> = (at..end).map(byte).collect();
            send_all(&client, &piece).await;
            at = end;
        }
        client
    }));
    let got = reader.join().unwrap();
    let first = got.iter().enumerate().position(|(i, b)| *b != byte(i));
    assert_eq!(first, None, "the bytes came out of order");
}

#[test]
fn an_owned_write_from_another_thread_stays_whole_among_the_owners_writes() {
    const BLOCKS: usize = 16;
    const BLOCK: usize = 64 << 10;
    const OWN: usize = 20_000;
    let rt = runtime();
    let (client, server) = rt.block_on(rt.spawn(pair()));
    let client = Arc::new(client);
    let reader =
        std::thread::spawn(move || nuclei::block_on(recv_exact(&server, BLOCKS * BLOCK + OWN)));
    let foreign = {
        let client = Arc::clone(&client);
        std::thread::spawn(move || {
            nuclei::block_on(async move {
                for k in 0..BLOCKS {
                    let (written, _) = client.write_owned(vec![0x80 | k as u8; BLOCK]).await;
                    assert_eq!(written.unwrap(), BLOCK);
                }
            })
        })
    };
    rt.block_on(rt.spawn({
        let client = Arc::clone(&client);
        async move {
            for i in 0..OWN {
                send_all(&client, &[(i % 128) as u8]).await;
            }
        }
    }));
    foreign.join().unwrap();
    let got = reader.join().unwrap();
    let mut owner = 0;
    let mut at = 0;
    while at < got.len() {
        let b = got[at];
        if b < 0x80 {
            assert_eq!(
                usize::from(b),
                owner % 128,
                "the owner's byte {owner} came out of order"
            );
            owner += 1;
            at += 1;
            continue;
        }
        let run = got[at..].iter().take_while(|x| **x == b).count();
        assert_eq!(
            run,
            BLOCK,
            "the owned write {} came apart at {at}",
            b & 0x7f
        );
        at += run;
    }
    assert_eq!(owner, OWN);
}
