//! TCP reads of any size go straight into the caller's buffer: a read that
//! fills its buffer exactly still knows whether the socket is empty (the next
//! one waits for the peer instead of finding nothing, or reads again at once
//! if bytes are left), and the end of a stream behind an exact read is seen.
//! Run on both backends (`NUCLEI_BACKEND=epoll`).

use std::time::Duration;

use nuclei::net::{TcpListener, TcpStream};

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

async fn send_all(conn: &TcpStream, bytes: &[u8]) {
    let mut sent = 0;
    while sent < bytes.len() {
        sent += conn.send(&bytes[sent..]).await.unwrap();
    }
}

async fn recv_exact(conn: &TcpStream, len: usize) -> Vec<u8> {
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
        let (client, server) = pair().await;
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
        let (client, server) = pair().await;
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
            let (client, server) = pair().await;
            send_all(&client, &[9u8; 64]).await;
            drop(client);
            assert_eq!(recv_exact(&server, 64).await, [9u8; 64]);
            let mut buf = [0u8; 64];
            assert_eq!(server.recv(&mut buf).await.unwrap(), 0);
        }
    });
}

#[test]
fn a_read_waits_for_bytes_that_come_later() {
    within(async {
        let (client, server) = pair().await;
        for round in 0..100u8 {
            let (got, ()) = nuclei::join!(recv_exact(&server, 64), async {
                nuclei::time::sleep(Duration::from_millis(1)).await;
                send_all(&client, &[round; 64]).await;
            });
            assert_eq!(got, [round; 64]);
        }
    });
}
