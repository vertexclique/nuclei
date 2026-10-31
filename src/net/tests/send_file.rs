//! `send_file`: ranges of a file arrive whole and in order, on TCP and on a
//! Unix stream, in the runtime's chunks of any size, and a dropped call
//! leaves the stream a prefix.

use super::*;
use crate::fs::File;

fn file_of(tag: &str, len: usize) -> (std::path::PathBuf, Vec<u8>) {
    static NEXT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nuclei-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data");
    let data = pattern(len);
    std::fs::write(&path, &data).unwrap();
    (path, data)
}

async fn tcp_pair() -> (Handle<TcpStream>, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = within(Handle::<TcpStream>::connect(listener.local_addr().unwrap()))
        .await
        .unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

/// Reads `want` bytes from `peer` on a thread, so the sender is never short
/// of a reader.
fn read_on_thread<R: Read + Send + 'static>(
    mut peer: R,
    want: usize,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut got = vec![0u8; want];
        peer.read_exact(&mut got).unwrap();
        got
    })
}

#[test]
fn a_runtimes_send_file_chunk_sends_every_byte_in_order() {
    for chunk in [1usize, 1000, 4096] {
        let rt = Builder::new()
            .worker_threads(1)
            .pin_workers(false)
            .send_file_chunk(chunk)
            .build()
            .unwrap();
        rt.block_on(async {
            let len = if chunk == 1 { 3001 } else { 100_003 };
            let (path, data) = file_of("sf-chunk", len);
            let file = File::open(&path).await.unwrap();
            let (tx, rx) = tcp_pair().await;
            let reader = read_on_thread(rx, len - 7);
            let sent = within(tx.send_file(&file, 7, len as u64)).await.unwrap();
            assert_eq!(sent, len as u64 - 7, "chunk {chunk}");
            assert!(
                reader.join().unwrap() == data[7..],
                "the bytes changed, chunk {chunk}"
            );
        });
    }
}

#[test]
fn send_file_sends_a_whole_file_larger_than_a_pipe() {
    let rt = runtime();
    rt.block_on(async {
        for len in [1usize, 4095, 65_536, 65_537, 3 * 1024 * 1024 + 11] {
            let (path, data) = file_of("sf-whole", len);
            let file = File::open(&path).await.unwrap();
            let (tx, rx) = tcp_pair().await;
            let reader = read_on_thread(rx, len);
            let sent = within(tx.send_file(&file, 0, len as u64)).await.unwrap();
            assert_eq!(sent, len as u64);
            assert!(
                reader.join().unwrap() == data,
                "the bytes changed, len {len}"
            );
        }
    });
}

#[test]
fn send_file_sends_ranges_at_offsets_and_stops_at_the_end_of_the_file() {
    let rt = runtime();
    rt.block_on(async {
        let (path, data) = file_of("sf-range", 300_000);
        let file = File::open(&path).await.unwrap();
        let (tx, rx) = tcp_pair().await;
        let want: Vec<u8> = [&data[10..1010], &data[100_000..230_000], &data[299_990..]].concat();
        let reader = read_on_thread(rx, want.len());
        assert_eq!(within(tx.send_file(&file, 10, 1000)).await.unwrap(), 1000);
        assert_eq!(
            within(tx.send_file(&file, 100_000, 130_000)).await.unwrap(),
            130_000
        );
        // Asks for more than the file holds from the offset.
        assert_eq!(
            within(tx.send_file(&file, 299_990, 5_000)).await.unwrap(),
            10
        );
        assert_eq!(within(tx.send_file(&file, 0, 0)).await.unwrap(), 0);
        assert_eq!(within(tx.send_file(&file, 400_000, 10)).await.unwrap(), 0);
        assert!(reader.join().unwrap() == want);
    });
}

#[test]
fn send_file_goes_after_what_was_written_and_before_what_is_written_next() {
    let rt = runtime();
    rt.block_on(async {
        let (path, data) = file_of("sf-order", 200_000);
        let file = File::open(&path).await.unwrap();
        let (tx, rx) = tcp_pair().await;
        let want = [&b"head"[..], &data[..], &b"tail"[..]].concat();
        let reader = read_on_thread(rx, want.len());
        let mut tx_ref = &tx;
        write_all(&mut tx_ref, b"head").await.unwrap();
        within(tx.send_file(&file, 0, 200_000)).await.unwrap();
        write_all(&mut tx_ref, b"tail").await.unwrap();
        assert!(reader.join().unwrap() == want);
    });
}

#[test]
fn a_dropped_send_file_leaves_the_stream_a_prefix_of_the_range() {
    let rt = runtime();
    rt.block_on(async {
        let (path, data) = file_of("sf-drop", 24 * 1024 * 1024);
        let file = File::open(&path).await.unwrap();
        let (tx, mut rx) = tcp_pair().await;
        {
            // Nobody reads: the socket fills and the call waits.
            let mut call = Box::pin(tx.send_file(&file, 0, data.len() as u64));
            for _ in 0..50 {
                let polled = poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx))).await;
                assert!(polled.is_pending(), "the call ended with no reader");
                crate::time::sleep(Duration::from_millis(2)).await;
            }
        }
        // Whatever reached the peer is the file's first bytes, whole.
        crate::time::sleep(Duration::from_millis(50)).await;
        rx.set_nonblocking(true).unwrap();
        let mut got = Vec::new();
        let mut chunk = vec![0u8; 1 << 16];
        loop {
            match rx.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("{err}"),
            }
        }
        assert!(!got.is_empty(), "nothing was sent");
        assert!(got.len() < data.len(), "everything was sent");
        assert!(got == data[..got.len()], "what arrived is not a prefix");
    });
}

#[cfg(unix)]
#[test]
fn send_file_on_a_unix_stream_and_on_something_that_is_no_file() {
    let rt = runtime();
    rt.block_on(async {
        let (path, data) = file_of("sf-unix", 500_000);
        let file = File::open(&path).await.unwrap();
        let (a, b) = UnixStream::pair().unwrap();
        let tx = Handle::new(a).unwrap();
        let reader = read_on_thread(b, data.len());
        assert_eq!(
            within(tx.send_file(&file, 0, data.len() as u64))
                .await
                .unwrap(),
            500_000
        );
        assert!(reader.join().unwrap() == data);
        // A directory names no bytes: the call fails, and the stream is whole.
        let dir = File::open(path.parent().unwrap()).await.unwrap();
        assert!(within(tx.send_file(&dir, 0, 10)).await.is_err());
    });
}
