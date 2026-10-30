//! Borrowed buffers big enough go straight between the caller's memory and
//! the kernel: every byte arrives in order whatever the sizes mix, a read or
//! write dropped before it ends loses and repeats nothing, and a handle
//! polled from other threads does the same. Run on both backends
//! (`NUCLEI_BACKEND=epoll`).

use std::sync::Arc;
use std::time::Duration;

use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream};

/// Bigger than any size a direct call starts at.
const LARGE: usize = 256 << 10;

fn within<F: core::future::Future>(f: F) -> F::Output {
    nuclei::block_on(async {
        nuclei::time::timeout(Duration::from_secs(60), f)
            .await
            .expect("nothing came within a minute")
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

/// Byte `at` of the stream every test sends.
fn byte(at: usize) -> u8 {
    (at.wrapping_mul(31) ^ (at >> 8)) as u8
}

fn pattern(from: usize, len: usize) -> Vec<u8> {
    (from..from + len).map(byte).collect()
}

/// Sends `total` bytes in writes of `chunks` in turn.
async fn send_all(conn: &TcpStream, total: usize, chunks: &[usize]) {
    let mut at = 0;
    for chunk in chunks.iter().cycle() {
        if at == total {
            break;
        }
        let data = pattern(at, (*chunk).min(total - at));
        let mut sent = 0;
        while sent < data.len() {
            sent += conn.send(&data[sent..]).await.unwrap();
        }
        at += data.len();
    }
}

/// Receives `total` bytes in reads of `sizes` in turn, checking each.
async fn recv_all(conn: &TcpStream, total: usize, sizes: &[usize]) {
    let mut at = 0;
    for size in sizes.iter().cycle() {
        if at == total {
            break;
        }
        let mut buf = vec![0u8; *size];
        let n = conn.recv(&mut buf).await.unwrap();
        assert_ne!(n, 0, "the stream ended at {at} of {total}");
        assert_eq!(buf[..n], pattern(at, n)[..], "bytes differ at {at}");
        at += n;
    }
}

#[test]
fn large_buffers_both_ways() {
    within(async {
        for round in 0..4 {
            let (client, server) = pair().await;
            let total = 3 << 20;
            let big = [LARGE, 1 << 20, 64 << 10];
            let ((), ()) = nuclei::join!(
                send_all(&client, total, &big),
                recv_all(&server, total, &big)
            );
            // And back, the other half's roles swapped.
            let ((), ()) = nuclei::join!(
                send_all(&server, total + round, &big),
                recv_all(&client, total + round, &big)
            );
        }
    });
}

#[test]
fn mixed_sizes_keep_the_order() {
    within(async {
        let (client, server) = pair().await;
        let total = 6 << 20;
        let ((), ()) = nuclei::join!(
            send_all(
                &client,
                total,
                &[100, LARGE, 7, 70_000, 1, 33_333, 1 << 20, 5]
            ),
            recv_all(&server, total, &[3, 128 << 10, 64, 1, 300_000, 17, 40_000])
        );
    });
}

#[test]
fn a_large_read_dropped_early_loses_nothing() {
    within(async {
        let (client, server) = pair().await;
        let mut buf = vec![0u8; LARGE];
        for _ in 0..5 {
            let early =
                nuclei::time::timeout(Duration::from_millis(5), server.recv(&mut buf)).await;
            assert!(early.is_err(), "nothing was sent yet");
        }
        // Bytes sent after a dropped read all come, in order, to the next.
        let total = 2 << 20;
        let big = [LARGE, 1 << 20];
        let ((), ()) = nuclei::join!(
            send_all(&client, total, &big),
            recv_all(&server, total, &[LARGE, 4096, 1 << 20])
        );
        // A read dropped while bytes keep coming.
        let sender = send_all(&client, total, &[1 << 20, 10]);
        let receiver = async {
            let mut buf = vec![0u8; LARGE];
            let mut at = 0;
            let mut cut = 0;
            while at < total {
                let read = nuclei::time::timeout(Duration::from_micros(200), server.recv(&mut buf));
                match read.await {
                    Ok(n) => {
                        let n = n.unwrap();
                        assert_eq!(buf[..n], pattern(at, n)[..], "bytes differ at {at}");
                        at += n;
                    }
                    Err(_) => cut += 1,
                }
            }
            cut
        };
        let ((), _) = nuclei::join!(sender, receiver);
    });
}

#[test]
fn a_large_write_dropped_midway_loses_and_repeats_nothing() {
    within(async {
        let (client, server) = pair().await;
        // The server reads nothing: the socket fills and a write waits.
        let mut sent = 0usize;
        let chunk = pattern(0, 1 << 20);
        loop {
            let at = sent % chunk.len();
            let part = &chunk[at..];
            let write = nuclei::time::timeout(Duration::from_millis(50), client.send(part));
            match write.await {
                Ok(n) => {
                    let n = n.unwrap();
                    assert!(n > 0);
                    sent += n;
                }
                Err(_) => break,
            }
        }
        // The stream is whole: `sent` bytes of the pattern, then "end".
        let finish = async { client.send(b"end").await.unwrap() };
        let drain = async {
            let mut got = 0;
            let mut buf = vec![0u8; LARGE];
            while got < sent {
                let n = server
                    .recv(&mut buf[..LARGE.min(sent - got)])
                    .await
                    .unwrap();
                assert_ne!(n, 0);
                for (i, b) in buf[..n].iter().enumerate() {
                    assert_eq!(*b, byte((got + i) % chunk.len()), "byte {}", got + i);
                }
                got += n;
            }
            let mut tail = [0u8; 3];
            recv_exact(&server, &mut tail).await;
            assert_eq!(&tail, b"end");
        };
        let (_, ()) = nuclei::join!(finish, drain);
    });
}

async fn recv_exact(conn: &TcpStream, buf: &mut [u8]) {
    let mut at = 0;
    while at < buf.len() {
        at += conn.recv(&mut buf[at..]).await.unwrap();
    }
}

#[test]
fn async_read_write_use_large_buffers() {
    within(async {
        let (client, server) = pair().await;
        let total = 2 << 20;
        let data = pattern(0, total);
        let write = async {
            let mut conn = &client;
            conn.write_all(&data).await.unwrap();
            conn.flush().await.unwrap();
        };
        let read = async {
            let mut conn = &server;
            let mut got = vec![0u8; total];
            conn.read_exact(&mut got).await.unwrap();
            got
        };
        let ((), got) = nuclei::join!(write, read);
        assert!(got == data, "the bytes differ");
    });
}

#[test]
fn a_handle_polled_from_other_threads() {
    let (client, server) = within(pair());
    let (client, server) = (Arc::new(client), Arc::new(server));
    let total = 4 << 20;
    let big = [LARGE, 64 << 10, 1 << 20];
    for (from, to) in [(&client, &server), (&server, &client)] {
        let (from, to) = (Arc::clone(from), Arc::clone(to));
        let sender = std::thread::spawn(move || nuclei::block_on(send_all(&from, total, &big)));
        let receiver = std::thread::spawn(move || nuclei::block_on(recv_all(&to, total, &big)));
        sender.join().unwrap();
        receiver.join().unwrap();
    }
}
