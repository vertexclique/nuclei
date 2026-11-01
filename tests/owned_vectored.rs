//! Owned vectored IO: several of the caller's buffers written in order as
//! one write, or read into the first that has room, on TCP, Unix streams,
//! pipes and files, with `Vec<Vec<u8>>`, arrays and aligned buffers.

use core::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use nuclei::fs::File;
use nuclei::io::{AlignedBuf, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream};

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nuclei-owned-vectored-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

async fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (client, server)
}

async fn read_all(stream: &TcpStream, len: usize) -> Vec<u8> {
    let mut got = Vec::with_capacity(len);
    while got.len() < len {
        let (n, back) = stream.read_owned(got).await;
        assert_ne!(n.unwrap(), 0, "the stream ended early");
        got = back;
    }
    got
}

#[test]
fn a_vectored_write_sends_every_buffer_in_order_for_every_size() {
    nuclei::block_on(async {
        for sizes in [
            vec![1, 1],
            vec![0, 5, 0],
            vec![4096, 1],
            vec![64 << 10, 64 << 10, 7],
            vec![3 << 20, 1 << 20, 2 << 20],
        ] {
            let (client, server) = tcp_pair().await;
            let bufs: Vec<Vec<u8>> = sizes
                .iter()
                .enumerate()
                .map(|(i, &len)| pattern(len, i as u8))
                .collect();
            let want: Vec<u8> = bufs.concat();
            let ptrs: Vec<_> = bufs.iter().map(|b| b.as_ptr()).collect();
            let total = want.len();
            let reader = nuclei::spawn(async move { read_all(&server, total).await });
            let (n, back): (io::Result<usize>, Vec<Vec<u8>>) =
                client.write_vectored_owned(bufs).await;
            assert_eq!(n.unwrap(), total, "sizes {sizes:?}");
            assert_eq!(back.iter().map(|b| b.as_ptr()).collect::<Vec<_>>(), ptrs);
            assert_eq!(reader.await, want, "sizes {sizes:?}");
        }
    });
}

#[test]
fn an_empty_vectored_write_writes_nothing() {
    nuclei::block_on(async {
        let (client, _server) = tcp_pair().await;
        let (n, back) = client.write_vectored_owned(Vec::<Vec<u8>>::new()).await;
        assert_eq!((n.unwrap(), back.len()), (0, 0));
        let (n, back) = client.write_vectored_owned([Vec::new(), Vec::new()]).await;
        assert_eq!((n.unwrap(), back.len()), (0, 2));
    });
}

#[test]
fn two_tasks_vectored_writes_never_interleave() {
    nuclei::block_on(async {
        let (client, server) = tcp_pair().await;
        let client = std::sync::Arc::new(client);
        const PARTS: usize = 3;
        const PART: usize = 256 << 10;
        const WRITES: usize = 4;
        let mut tasks = Vec::new();
        for seed in *b"ab" {
            let client = std::sync::Arc::clone(&client);
            tasks.push(nuclei::spawn(async move {
                for _ in 0..WRITES {
                    let bufs: [Vec<u8>; PARTS] = core::array::from_fn(|_| vec![seed; PART]);
                    let (n, _) = client.write_vectored_owned(bufs).await;
                    assert_eq!(n.unwrap(), PARTS * PART);
                }
            }));
        }
        let got = read_all(&server, 2 * WRITES * PARTS * PART).await;
        for task in tasks {
            task.await;
        }
        for write in got.chunks(PARTS * PART) {
            assert!(
                write.iter().all(|&b| b == write[0]),
                "another write came between the buffers of one"
            );
        }
    });
}

#[test]
fn a_vectored_read_fills_each_buffer_before_the_next_and_keeps_what_they_held() {
    nuclei::block_on(async {
        let (client, server) = tcp_pair().await;
        let (n, _) = client.write_owned(b"0123456789".to_vec()).await;
        assert_eq!(n.unwrap(), 10);
        let mut got = Vec::new();
        let mut held = Vec::with_capacity(4);
        held.extend_from_slice(b"ab");
        let mut bufs = vec![
            held,
            Vec::with_capacity(0),
            Vec::with_capacity(3),
            Vec::with_capacity(64),
        ];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while got.len() < 10 {
            assert!(std::time::Instant::now() < deadline, "the bytes never came");
            let lens: Vec<usize> = bufs.iter().map(Vec::len).collect();
            let (n, back) = server.read_vectored_owned(bufs).await;
            let n = n.unwrap();
            assert_ne!(n, 0);
            let mut short = false;
            for (buf, len) in back.iter().zip(&lens) {
                let gained = buf.len() - len;
                assert!(
                    !(short && gained > 0),
                    "a later buffer filled before an earlier one"
                );
                short |= buf.len() < buf.capacity();
                got.extend_from_slice(&buf[*len..]);
            }
            assert_eq!(back[0][..2], *b"ab");
            bufs = back;
        }
        assert_eq!(got, b"0123456789");
        let full = [AlignedBuf::zeroed(8, 8).unwrap()];
        let (n, back) = server.read_vectored_owned(full).await;
        assert_eq!((n.unwrap(), back[0].len()), (0, 8), "no room is no read");
    });
}

#[test]
fn a_vectored_read_of_a_closed_stream_reads_nothing() {
    nuclei::block_on(async {
        let (client, server) = tcp_pair().await;
        drop(client);
        let (n, back) = server
            .read_vectored_owned(vec![AlignedBuf::new(16, 16).unwrap()])
            .await;
        assert_eq!((n.unwrap(), back[0].capacity()), (0, 16));
    });
}

#[cfg(unix)]
#[test]
fn unix_streams_and_pipes_take_vectored_buffers() {
    use nuclei::net::UnixStream;
    nuclei::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        let (n, _) = a
            .write_vectored_owned(vec![b"unix ".to_vec(), b"stream".to_vec()])
            .await;
        assert_eq!(n.unwrap(), 11);
        let mut got = Vec::new();
        while got.len() < 11 {
            let (n, back) = b.read_vectored_owned([Vec::with_capacity(4)]).await;
            assert_ne!(n.unwrap(), 0);
            got.extend_from_slice(&back[0]);
        }
        assert_eq!(got, b"unix stream");

        // Bytes queued before the read: one read fills both buffers.
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        std::io::Write::write_all(&mut &a, b"queued bytes").unwrap();
        let b = nuclei::io::Handle::new(b).unwrap();
        let (n, back) = b
            .read_vectored_owned([Vec::with_capacity(5), Vec::with_capacity(32)])
            .await;
        assert_eq!(n.unwrap(), 12);
        assert_eq!(
            (&back[0][..], &back[1][..]),
            (&b"queue"[..], &b"d bytes"[..])
        );

        let (r, w) = std::io::pipe().unwrap();
        let (r, w) = (
            nuclei::io::Handle::new(r).unwrap(),
            nuclei::io::Handle::new(w).unwrap(),
        );
        let (n, _) = w
            .write_vectored_owned([b"pi".to_vec(), b"pe".to_vec()])
            .await;
        assert_eq!(n.unwrap(), 4);
        drop(w);
        let (n, back) = r
            .read_vectored_owned(vec![
                Vec::with_capacity(0),
                Vec::with_capacity(3),
                Vec::with_capacity(8),
            ])
            .await;
        assert_eq!(n.unwrap(), 4, "one read of every room");
        assert_eq!((&back[1][..], &back[2][..]), (&b"pip"[..], &b"e"[..]));
    });
}

#[test]
fn a_files_vectored_write_lands_at_the_cursor_after_the_writes_before_it() {
    nuclei::block_on(async {
        let t = Temp::new();
        let mut file = File::create_new(t.path("v")).await.unwrap();
        file.write_all(b"head:").await.unwrap();
        let big = pattern(300 << 10, 9);
        let bufs = vec![b"one,".to_vec(), Vec::new(), big.clone(), b",two".to_vec()];
        let (n, back) = file.write_vectored_owned(bufs).await;
        assert_eq!(n.unwrap(), 4 + big.len() + 4);
        assert_eq!(back[2].len(), big.len());
        file.write_all(b":tail").await.unwrap();
        file.flush().await.unwrap();
        let mut want = b"head:one,".to_vec();
        want.extend_from_slice(&big);
        want.extend_from_slice(b",two:tail");
        assert_eq!(std::fs::read(t.path("v")).unwrap(), want);
    });
}

#[test]
fn a_files_vectored_write_appends_every_buffer_at_the_end() {
    nuclei::block_on(async {
        let t = Temp::new();
        std::fs::write(t.path("a"), b"start").unwrap();
        let file = nuclei::fs::OpenOptions::new()
            .append(true)
            .open(t.path("a"))
            .await
            .unwrap();
        let (n, _) = file
            .write_vectored_owned([b"-x".to_vec(), b"-y".to_vec(), b"-z".to_vec()])
            .await;
        assert_eq!(n.unwrap(), 6);
        assert_eq!(std::fs::read(t.path("a")).unwrap(), b"start-x-y-z");
    });
}

#[test]
fn a_files_vectored_read_fills_its_buffers_in_order_at_the_cursor() {
    nuclei::block_on(async {
        let t = Temp::new();
        std::fs::write(t.path("r"), b"abcdefghij").unwrap();
        let file = File::open(t.path("r")).await.unwrap();
        let mut held = Vec::with_capacity(4);
        held.extend_from_slice(b">>");
        let (n, back) = file
            .read_vectored_owned([held, Vec::with_capacity(3), Vec::with_capacity(2)])
            .await;
        assert_eq!(n.unwrap(), 7);
        assert_eq!(
            (&back[0][..], &back[1][..], &back[2][..]),
            (&b">>ab"[..], &b"cde"[..], &b"fg"[..])
        );
        let (n, back) = file
            .read_vectored_owned(vec![Vec::with_capacity(1), Vec::with_capacity(16)])
            .await;
        assert_eq!(n.unwrap(), 3);
        assert_eq!((&back[0][..], &back[1][..]), (&b"h"[..], &b"ij"[..]));
        let (n, _) = file.read_vectored_owned([Vec::with_capacity(4)]).await;
        assert_eq!(n.unwrap(), 0, "the end of the file");
    });
}

#[test]
fn a_files_vectored_read_after_a_buffered_read_and_write_sees_the_write() {
    nuclei::block_on(async {
        let t = Temp::new();
        std::fs::write(t.path("w"), b"0123456789").unwrap();
        let mut file = nuclei::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(t.path("w"))
            .await
            .unwrap();
        let mut one = [0u8; 1];
        nuclei::io::AsyncReadExt::read_exact(&mut file, &mut one)
            .await
            .unwrap();
        assert_eq!(&one, b"0");
        file.write_all(b"AB").await.unwrap();
        let (n, back) = file
            .read_vectored_owned([
                Vec::with_capacity(2),
                Vec::with_capacity(2),
                Vec::with_capacity(9),
            ])
            .await;
        assert_eq!(n.unwrap(), 7);
        assert_eq!(
            (&back[0][..], &back[1][..], &back[2][..]),
            (&b"34"[..], &b"56"[..], &b"789"[..])
        );
    });
}

#[test]
fn many_small_buffers_past_one_calls_limit_are_written_whole() {
    nuclei::block_on(async {
        let (client, server) = tcp_pair().await;
        let bufs: Vec<Vec<u8>> = (0..2500).map(|i| pattern(1 + i % 37, i as u8)).collect();
        let want = bufs.concat();
        let total = want.len();
        let reader = nuclei::spawn(async move { read_all(&server, total).await });
        let (n, back) = client.write_vectored_owned(bufs).await;
        assert_eq!((n.unwrap(), back.len()), (total, 2500));
        assert_eq!(reader.await, want);

        let t = Temp::new();
        let file = File::create_new(t.path("many")).await.unwrap();
        let bufs: Vec<Vec<u8>> = (0..1500).map(|i| pattern(1 + i % 5, i as u8)).collect();
        let want = bufs.concat();
        let (n, _) = file.write_vectored_owned(bufs).await;
        assert_eq!(n.unwrap(), want.len());
        file.sync_all().await.unwrap();
        assert_eq!(std::fs::read(t.path("many")).unwrap(), want);
        let reader = File::open(t.path("many")).await.unwrap();
        let rooms: Vec<Vec<u8>> = (0..1200).map(|_| Vec::with_capacity(3)).collect();
        let (n, back) = reader.read_vectored_owned(rooms).await;
        let n = n.unwrap();
        assert!(n > 0 && n <= 3600);
        assert_eq!(back.concat(), want[..n]);
    });
}

#[test]
fn a_dropped_vectored_file_write_still_lands_whole() {
    nuclei::block_on(async {
        let t = Temp::new();
        let file = File::create_new(t.path("d")).await.unwrap();
        let bufs = vec![pattern(1 << 20, 1), pattern(1 << 20, 2)];
        let want = bufs.concat();
        let mut write = Box::pin(file.write_vectored_owned(bufs));
        let _ = core::future::poll_fn(|cx| {
            let _ = write.as_mut().poll(cx);
            core::task::Poll::Ready(())
        })
        .await;
        drop(write);
        file.sync_all().await.unwrap();
        assert_eq!(std::fs::read(t.path("d")).unwrap(), want);
    });
}

#[cfg(target_os = "linux")]
#[test]
fn a_direct_files_vectored_write_takes_aligned_buffers() {
    use std::os::unix::fs::OpenOptionsExt;
    let t = Temp::new();
    let path = t.path("direct");
    let std_file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_DIRECT)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) => {
            eprintln!("skipping: {} refuses O_DIRECT: {err}", t.0.display());
            return;
        }
    };
    nuclei::block_on(async {
        let file = nuclei::io::Handle::new(std_file).unwrap();
        let mut a = AlignedBuf::zeroed(4096, 4096).unwrap();
        a.fill(1);
        let mut b = AlignedBuf::zeroed(8192, 4096).unwrap();
        b.fill(2);
        let (n, back) = file.write_vectored_owned([a, b]).await;
        assert_eq!(n.unwrap(), 12288);
        assert_eq!(back[1].alignment(), 4096);
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes[..4096].iter().all(|&x| x == 1));
        assert!(bytes[4096..].iter().all(|&x| x == 2));
    });
}
