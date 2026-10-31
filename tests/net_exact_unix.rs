//! Unix stream reads of any size go straight into the caller's buffer where
//! the kernel counts what a read leaves (`SO_INQ`, Linux 6.17), and through the
//! read-ahead where it does not: either way a read that fills its buffer
//! exactly knows whether the socket is empty, and the end of a stream behind
//! an exact read is seen, whether it came with the bytes or after them. Run on
//! both backends (`NUCLEI_BACKEND=epoll`).

use std::time::Duration;

use nuclei::net::UnixStream;

fn within<F: core::future::Future>(f: F) -> F::Output {
    nuclei::block_on(async {
        nuclei::time::timeout(Duration::from_secs(60), f)
            .await
            .expect("nothing came within a minute")
    })
}

async fn send_all(conn: &UnixStream, bytes: &[u8]) {
    let mut sent = 0;
    while sent < bytes.len() {
        sent += conn.send(&bytes[sent..]).await.unwrap();
    }
}

async fn recv_exact(conn: &UnixStream, len: usize) -> Vec<u8> {
    let mut got = vec![0u8; len];
    let mut at = 0;
    while at < len {
        let n = conn.recv(&mut got[at..]).await.unwrap();
        assert_ne!(n, 0, "the stream ended at {at} of {len}");
        at += n;
    }
    got
}

#[test]
fn exact_reads_ping_pong() {
    within(async {
        let (client, server) = UnixStream::pair().unwrap();
        let ((), ()) = nuclei::join!(
            async {
                for round in 0..2000u32 {
                    let msg = [round as u8; 64];
                    send_all(&client, &msg).await;
                    assert_eq!(recv_exact(&client, 64).await, msg);
                }
            },
            async {
                for round in 0..2000u32 {
                    let got = recv_exact(&server, 64).await;
                    assert_eq!(got, [round as u8; 64]);
                    send_all(&server, &got).await;
                }
            }
        );
    });
}

#[test]
fn exact_reads_take_a_message_piece_by_piece() {
    within(async {
        let (client, server) = UnixStream::pair().unwrap();
        let msg: Vec<u8> = (0..192u32).map(|i| i as u8).collect();
        for _ in 0..200 {
            send_all(&client, &msg).await;
            for piece in msg.chunks(64) {
                let mut buf = [0u8; 64];
                let mut at = 0;
                while at < buf.len() {
                    at += server.recv(&mut buf[at..]).await.unwrap();
                }
                assert_eq!(buf, piece);
            }
        }
    });
}

#[test]
fn the_end_of_a_stream_is_seen_behind_an_exact_read() {
    within(async {
        for _ in 0..50 {
            let (client, server) = UnixStream::pair().unwrap();
            send_all(&client, &[9u8; 64]).await;
            drop(client);
            assert_eq!(recv_exact(&server, 64).await, [9u8; 64]);
            let mut buf = [0u8; 64];
            assert_eq!(server.recv(&mut buf).await.unwrap(), 0);
        }
    });
}

#[test]
fn an_end_after_an_exact_read_is_seen() {
    within(async {
        for _ in 0..50 {
            let (client, server) = UnixStream::pair().unwrap();
            send_all(&client, &[3u8; 64]).await;
            assert_eq!(recv_exact(&server, 64).await, [3u8; 64]);
            let mut buf = [0u8; 64];
            let (n, ()) = nuclei::join!(server.recv(&mut buf), async {
                nuclei::time::sleep(Duration::from_millis(1)).await;
                drop(client);
            });
            assert_eq!(n.unwrap(), 0);
        }
    });
}

#[test]
fn a_read_waits_for_bytes_that_come_later() {
    within(async {
        let (client, server) = UnixStream::pair().unwrap();
        for round in 0..100u8 {
            let (got, ()) = nuclei::join!(recv_exact(&server, 64), async {
                nuclei::time::sleep(Duration::from_millis(1)).await;
                send_all(&client, &[round; 64]).await;
            });
            assert_eq!(got, [round; 64]);
        }
    });
}
