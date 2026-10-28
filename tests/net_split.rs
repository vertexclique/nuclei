//! A stream's halves: borrowed ones read and write together, owned ones on
//! tasks of their own and made one stream again; halves of two streams are
//! not.

use std::time::Duration;

use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream};

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
fn borrowed_halves_read_and_write_at_once() {
    within(async {
        let (client, server) = pair().await;
        let (mut read, mut write) = server.split();
        assert_eq!(
            read.as_ref().peer_addr().unwrap(),
            client.local_addr().unwrap()
        );
        // The server echoes what it reads while the client writes.
        let echo = async {
            let mut buf = [0u8; 5];
            read.read_exact(&mut buf).await.unwrap();
            write.write_all(&buf).await.unwrap();
            write.flush().await.unwrap();
        };
        let talk = async {
            let mut client = &client;
            client.write_all(b"hello").await.unwrap();
            client.flush().await.unwrap();
            let mut back = [0u8; 5];
            client.read_exact(&mut back).await.unwrap();
            back
        };
        let ((), back) = nuclei::join!(echo, talk);
        assert_eq!(&back, b"hello");
    });
}

#[test]
fn owned_halves_go_to_tasks_and_come_back_one_stream() {
    within(async {
        let (client, server) = pair().await;
        let (mut read, mut write) = server.into_split();
        let reader = nuclei::spawn(async move {
            let mut buf = [0u8; 4];
            read.read_exact(&mut buf).await.unwrap();
            (read, buf)
        });
        let writer = nuclei::spawn(async move {
            write.write_all(b"pong").await.unwrap();
            write.flush().await.unwrap();
            write
        });
        let mut c = &client;
        c.write_all(b"ping").await.unwrap();
        c.flush().await.unwrap();
        let mut got = [0u8; 4];
        c.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"pong");
        let (read, buf) = reader.await;
        assert_eq!(&buf, b"ping");
        let write = writer.await;
        let server = read.reunite(write).expect("the halves of one stream");
        assert_eq!(server.peer_addr().unwrap(), client.local_addr().unwrap());
    });
}

#[test]
fn halves_of_two_streams_are_not_one_stream() {
    within(async {
        let (a, _a_peer) = pair().await;
        let (b, _b_peer) = pair().await;
        let (a_read, a_write) = a.into_split();
        let (b_read, b_write) = b.into_split();
        let err = a_read.reunite(b_write).unwrap_err();
        assert_eq!(err.to_string(), "the two halves are of different streams");
        // Both come back, and each pairs with its own.
        let nuclei::net::ReuniteError(a_read, b_write) = err;
        assert!(a_read.reunite(a_write).is_ok());
        assert!(b_write.reunite(b_read).is_ok());
    });
}

#[cfg(unix)]
#[test]
fn a_unix_stream_splits_too() {
    within(async {
        let (a, b) = nuclei::net::UnixStream::pair().unwrap();
        let (mut read, _write) = b.split();
        let (_ar, mut aw) = a.into_split();
        aw.write_all(b"unix").await.unwrap();
        aw.flush().await.unwrap();
        let mut buf = [0u8; 4];
        read.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"unix");
    });
}
