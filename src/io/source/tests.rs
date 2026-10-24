//! Sources on real sockets, driven by a shard: the immediate path, the
//! driver's completions, what is kept for later, and the descriptor rules.

use std::future::{Future, poll_fn};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use super::write::Dest;
use super::{Addr, Kind, Source};
use crate::io::sys;
use crate::runtime::{Builder, Runtime};

/// A socket with its source, retired before the socket closes: the order a
/// handle keeps.
struct Owned<T: AsRawFd> {
    src: Arc<Source>,
    io: Option<T>,
}

impl<T: AsRawFd> Owned<T> {
    fn new(io: T, kind: Kind) -> Self {
        sys::set_nonblocking(io.as_raw_fd()).unwrap();
        Self {
            src: Source::new(io.as_raw_fd(), kind),
            io: Some(io),
        }
    }

    fn io(&self) -> &T {
        self.io.as_ref().unwrap()
    }
}

impl<T: AsRawFd> Drop for Owned<T> {
    fn drop(&mut self) {
        self.src.retire();
        drop(self.io.take());
    }
}

fn runtime(cap: Option<usize>) -> Runtime {
    let mut b = Builder::new().worker_threads(1).pin_workers(false);
    if let Some(cap) = cap {
        b = b.io_buffer_cap(cap);
    }
    b.build().unwrap()
}

fn pair() -> (Owned<UnixStream>, Owned<UnixStream>) {
    let (a, b) = UnixStream::pair().unwrap();
    (Owned::new(a, Kind::Stream), Owned::new(b, Kind::Stream))
}

async fn read(src: &Arc<Source>, buf: &mut [u8]) -> io::Result<usize> {
    poll_fn(|cx| src.poll_read(cx, buf, false)).await
}

/// `fut`, failing the test rather than hanging it.
async fn within<F: Future>(fut: F) -> F::Output {
    crate::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("the operation never completed")
}

/// Polls `fut` once: it must wait.
async fn pends<F: Future + Unpin>(fut: &mut F) {
    let polled = poll_fn(|cx| Poll::Ready(Pin::new(&mut *fut).poll(cx))).await;
    assert!(polled.is_pending(), "it did not wait");
}

async fn peek(src: &Arc<Source>, buf: &mut [u8]) -> io::Result<usize> {
    poll_fn(|cx| src.poll_read(cx, buf, true)).await
}

async fn write_all(src: &Arc<Source>, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = poll_fn(|cx| src.poll_write(cx, bytes)).await?;
        bytes = &bytes[n..];
    }
    Ok(())
}

async fn flush(src: &Arc<Source>) -> io::Result<()> {
    poll_fn(|cx| src.poll_flush(cx)).await
}

async fn read_exact(src: &Arc<Source>, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match read(src, buf).await? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => buf = &mut buf[n..],
        }
    }
    Ok(())
}

/// A pattern no shift of itself matches, so misordered bytes show.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// A waker that counts nothing and only records it was woken.
struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_ready_read_and_write_finish_at_once() {
    let rt = runtime(None);
    let (a, b) = pair();
    rt.block_on(async {
        write_all(&a.src, b"hello").await.unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(read(&b.src, &mut buf).await.unwrap(), 5);
        assert_eq!(&buf[..5], b"hello");
    });
}

#[test]
fn a_read_that_waits_is_completed_by_the_driver() {
    let rt = runtime(None);
    let (a, b) = pair();
    let writer = {
        let fd = a.io().as_raw_fd();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            // SAFETY: `a` outlives the thread, which is joined below.
            sys::write(fd, b"late").unwrap();
        })
    };
    rt.block_on(async {
        let mut buf = [0u8; 16];
        assert_eq!(within(read(&b.src, &mut buf)).await.unwrap(), 4);
        assert_eq!(&buf[..4], b"late");
    });
    writer.join().unwrap();
}

#[test]
fn bytes_read_ahead_for_a_dropped_read_go_to_the_next_one() {
    let rt = runtime(None);
    let (a, b) = pair();
    rt.block_on(async {
        // A read that waits, then is dropped.
        let mut buf = [0u8; 8];
        let mut first = Box::pin(read(&b.src, &mut buf));
        pends(&mut first).await;
        drop(first);
        a.io().write_all(b"kept").unwrap();
        // The driver reads the bytes for the dropped read.
        crate::time::sleep(Duration::from_millis(10)).await;
        let mut buf = [0u8; 8];
        assert_eq!(within(read(&b.src, &mut buf)).await.unwrap(), 4);
        assert_eq!(&buf[..4], b"kept");
    });
}

#[test]
fn a_peek_leaves_the_bytes_for_the_read() {
    let rt = runtime(None);
    let (a, b) = pair();
    rt.block_on(async {
        a.io().write_all(b"look").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(peek(&b.src, &mut buf).await.unwrap(), 4);
        assert_eq!(peek(&b.src, &mut buf).await.unwrap(), 4);
        assert_eq!(read(&b.src, &mut buf).await.unwrap(), 4);
        assert_eq!(&buf[..4], b"look");
    });
}

#[test]
fn the_end_of_a_stream_reads_as_zero() {
    let rt = runtime(None);
    let (a, b) = pair();
    rt.block_on(async move {
        let mut buf = [0u8; 8];
        let mut reader = Box::pin(read(&b.src, &mut buf));
        pends(&mut reader).await;
        // The driver sees the end.
        drop(a);
        assert_eq!(within(reader).await.unwrap(), 0);
    });
}

#[test]
fn writes_past_the_socket_buffer_arrive_whole_and_in_order() {
    let rt = runtime(None);
    let (a, b) = pair();
    let data = pattern(4 << 20);
    let expected = data.clone();
    let reader_src = Arc::clone(&b.src);
    rt.block_on(async move {
        let reader = crate::runtime::spawn(async move {
            let mut got = vec![0u8; expected.len()];
            read_exact(&reader_src, &mut got).await.unwrap();
            got == expected
        });
        within(write_all(&a.src, &data)).await.unwrap();
        within(flush(&a.src)).await.unwrap();
        assert!(
            within(reader).await,
            "the bytes arrived out of order or changed"
        );
    });
    drop(b);
}

#[test]
fn a_failed_write_behind_fails_every_write_until_a_flush_reports_it() {
    let rt = runtime(None);
    let (a, b) = pair();
    rt.block_on(async {
        // Fill the socket so a write goes behind.
        let chunk = vec![1u8; 64 * 1024];
        let start = Instant::now();
        while !a.src.has_write_behind() {
            within(write_all(&a.src, &chunk)).await.unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "nothing went behind"
            );
        }
        // The peer goes: the write-behind fails.
        drop(b);
        let failed = within(async {
            loop {
                match poll_fn(|cx| a.src.poll_write(cx, b"x")).await {
                    Err(err) => break err,
                    Ok(_) => crate::time::sleep(Duration::from_millis(1)).await,
                }
            }
        })
        .await;
        assert!(
            matches!(
                failed.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ),
            "{failed:?}"
        );
        assert!(
            poll_fn(|cx| a.src.poll_write(cx, b"x")).await.is_err(),
            "sticky"
        );
        assert!(within(flush(&a.src)).await.is_err(), "the flush reports it");
    });
}

#[test]
fn connections_are_accepted_now_and_by_the_driver_and_none_are_lost() {
    let rt = runtime(None);
    let dir = tempdir();
    let path = dir.join("accept.sock");
    let listener = Owned::new(UnixListener::bind(&path).unwrap(), Kind::UnixListener);
    rt.block_on(async {
        // An accept that waits, then is dropped: the driver still takes the
        // connection that comes, for the next accept.
        let mut dropped = Box::pin(poll_fn(|cx| listener.src.poll_accept(cx)));
        pends(&mut dropped).await;
        drop(dropped);
        let first = UnixStream::connect(&path).unwrap();
        crate::time::sleep(Duration::from_millis(10)).await;
        let (conn, addr) = within(poll_fn(|cx| listener.src.poll_accept(cx)))
            .await
            .unwrap();
        assert!(matches!(addr, Addr::Unix(_)));
        drop((conn, first));
        // Then a burst: some accepted at once, the rest by the driver.
        let mut waiting = Box::pin(poll_fn(|cx| listener.src.poll_accept(cx)));
        pends(&mut waiting).await;
        let clients: Vec<_> = (0..8)
            .map(|_| UnixStream::connect(&path).unwrap())
            .collect();
        let mut accepted = vec![within(waiting).await.unwrap()];
        while accepted.len() < clients.len() {
            accepted.push(
                within(poll_fn(|cx| listener.src.poll_accept(cx)))
                    .await
                    .unwrap(),
            );
        }
        // Each accepted connection is a working peer of one client.
        for (client, (conn, _)) in clients.iter().zip(&accepted) {
            let _ = (client, conn);
        }
        let mut seen = 0;
        for (conn, _) in &accepted {
            let conn = Owned::new(UnixStream::from(conn.try_clone().unwrap()), Kind::Stream);
            within(write_all(&conn.src, b"!")).await.unwrap();
            seen += 1;
        }
        assert_eq!(seen, clients.len());
        for mut client in clients {
            let mut b = [0u8; 1];
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"!");
        }
    });
    std::fs::remove_dir_all(dir).unwrap();
}

/// Linux lets a Unix datagram be as large as the socket buffer; other
/// systems cap them far lower.
#[cfg(target_os = "linux")]
#[test]
fn datagrams_wait_peek_and_arrive_whole_even_past_the_largest_class() {
    let rt = runtime(None);
    let (a, b) = UnixDatagram::pair().unwrap();
    // Room for one datagram larger than the pool's largest class.
    for s in [&a, &b] {
        set_buffers(s.as_raw_fd(), 1 << 20);
    }
    let a = Owned::new(a, Kind::UnixDatagram);
    let b = Owned::new(b, Kind::UnixDatagram);
    let big = pattern(150 * 1024);
    rt.block_on(async {
        let mut buf = vec![0u8; 256 * 1024];
        let mut recv = Box::pin(poll_fn(|cx| b.src.poll_receive(cx, &mut buf, true)));
        pends(&mut recv).await;
        let (sender, bytes) = (Arc::clone(&a.src), big.clone());
        let sent = crate::runtime::spawn(async move {
            poll_fn(|cx| sender.poll_send(cx, &bytes, Dest::Connected)).await
        });
        let (n, _) = within(recv).await.unwrap();
        assert_eq!(within(sent).await.unwrap(), big.len());
        assert_eq!(n, big.len(), "the datagram was cut");
        let mut buf = vec![0u8; 256 * 1024];
        let (n, _) = within(poll_fn(|cx| b.src.poll_receive(cx, &mut buf, false)))
            .await
            .unwrap();
        assert_eq!(&buf[..n], &big[..], "the peek left it whole");
    });
}

#[test]
fn a_datagram_is_cut_to_the_buffer_as_the_kernel_cuts_it() {
    let rt = runtime(None);
    let (a, b) = UnixDatagram::pair().unwrap();
    let a = Owned::new(a, Kind::UnixDatagram);
    let b = Owned::new(b, Kind::UnixDatagram);
    rt.block_on(async {
        let mut small = [0u8; 4];
        let mut recv = Box::pin(poll_fn(|cx| b.src.poll_receive(cx, &mut small, false)));
        pends(&mut recv).await;
        a.io().send(b"0123456789").unwrap();
        let (n, _) = within(recv).await.unwrap();
        assert_eq!(n, 4);
        assert_eq!(&small, b"0123");
        // The rest of that datagram is gone, as with the kernel's own cut.
        a.io().send(b"next").unwrap();
        let mut buf = [0u8; 16];
        let (n, _) = within(poll_fn(|cx| b.src.poll_receive(cx, &mut buf, false)))
            .await
            .unwrap();
        assert_eq!(&buf[..n], b"next");
    });
}

/// The descriptor number changes hands while the old open file lives on
/// (a dup elsewhere keeps the kernel's registration of it alive unless it
/// was deleted): neither the old file's events nor a stale token may make
/// the driver read the new socket's bytes.
#[test]
fn a_reused_descriptor_number_is_never_read_for_the_old_registration() {
    let rt = runtime(None);
    let (old_peer, mine) = UnixStream::pair().unwrap();
    sys::set_nonblocking(mine.as_raw_fd()).unwrap();
    let keep = mine.try_clone().unwrap();
    let src = Source::new(mine.as_raw_fd(), Kind::Stream);
    rt.block_on(async {
        let mut buf = [0u8; 8];
        let mut waiting = Box::pin(read(&src, &mut buf));
        pends(&mut waiting).await;
        drop(waiting);
    });
    // The handle lets go of its object: the source retires first.
    src.retire();
    let number = mine.into_raw_fd();
    // The number names another socket now, atomically: it is never free
    // for another thread of the test process to take.
    let (new_peer, other) = UnixStream::pair().unwrap();
    // SAFETY: `number` is this test's own descriptor; dup2 replaces it.
    assert_eq!(unsafe { libc::dup2(other.as_raw_fd(), number) }, number);
    drop(other);
    // SAFETY: `number` is this test's own descriptor.
    let reused = UnixStream::from(unsafe { OwnedFd::from_raw_fd(number) });
    reused.set_nonblocking(true).unwrap();
    (&old_peer).write_all(b"old").unwrap();
    (&new_peer).write_all(b"mine").unwrap();
    rt.block_on(crate::time::sleep(Duration::from_millis(20)));
    let mut buf = [0u8; 8];
    assert_eq!((&reused).read(&mut buf).unwrap(), 4);
    assert_eq!(&buf[..4], b"mine", "the new socket's bytes are all there");
    keep.set_nonblocking(true).unwrap();
    assert_eq!((&keep).read(&mut buf).unwrap(), 3);
    assert_eq!(&buf[..3], b"old");
    drop(src);
}

/// `get_mut` swaps the IO object: what the old one left is not handed out
/// as the new one's.
#[test]
fn a_revival_with_another_descriptor_discards_the_old_ones_results() {
    let rt = runtime(None);
    let (a1, b1) = UnixStream::pair().unwrap();
    let (a2, b2) = UnixStream::pair().unwrap();
    sys::set_nonblocking(b1.as_raw_fd()).unwrap();
    sys::set_nonblocking(b2.as_raw_fd()).unwrap();
    let src = Source::new(b1.as_raw_fd(), Kind::Stream);
    rt.block_on(async {
        let mut buf = [0u8; 8];
        let mut waiting = Box::pin(read(&src, &mut buf));
        pends(&mut waiting).await;
        drop(waiting);
        (&a1).write_all(b"old!").unwrap();
        crate::time::sleep(Duration::from_millis(10)).await;
        assert!(
            src.has_read_ahead(),
            "the driver read the old socket's bytes"
        );
    });
    // Swap: retire, then revive with the other socket.
    src.retire();
    drop(b1);
    src.revive(b2.as_raw_fd());
    (&a2).write_all(b"new").unwrap();
    rt.block_on(async {
        let mut buf = [0u8; 8];
        let n = within(read(&src, &mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"new");
    });
    src.retire();
    drop((a1, a2, b2));
}

/// A source revived with its own descriptor (a `get_mut` that changed
/// nothing) keeps what it read ahead.
#[test]
fn a_revival_with_the_same_descriptor_keeps_the_read_ahead() {
    let rt = runtime(None);
    let (a, b) = pair();
    rt.block_on(async {
        let mut buf = [0u8; 8];
        let mut waiting = Box::pin(read(&b.src, &mut buf));
        pends(&mut waiting).await;
        drop(waiting);
        a.io().write_all(b"ahead").unwrap();
        crate::time::sleep(Duration::from_millis(10)).await;
        assert!(b.src.has_read_ahead());
    });
    b.src.retire();
    b.src.revive(b.io().as_raw_fd());
    rt.block_on(async {
        let mut buf = [0u8; 8];
        let n = within(read(&b.src, &mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"ahead");
    });
}

/// A pool asked to hold nothing holds one small buffer: reads still
/// complete, all through it, as it comes back for each.
#[test]
fn reads_complete_through_a_pool_of_one_small_buffer() {
    let rt = runtime(Some(0));
    let (a, b) = pair();
    let data = pattern(256 * 1024);
    let expected = data.clone();
    let reader_src = Arc::clone(&b.src);
    rt.block_on(async move {
        let reader = crate::runtime::spawn(async move {
            let mut got = vec![0u8; expected.len()];
            read_exact(&reader_src, &mut got).await.unwrap();
            got == expected
        });
        let writer_fd = a.io().as_raw_fd();
        let writer = std::thread::spawn(move || {
            let mut rest = &data[..];
            while !rest.is_empty() {
                match sys::write(writer_fd, rest) {
                    Ok(n) => rest = &rest[n..],
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::yield_now(),
                    Err(e) => panic!("{e}"),
                }
            }
        });
        assert!(within(reader).await);
        writer.join().unwrap();
        drop(a);
    });
}

/// A task of another executor, on a thread that runs no shard: the source
/// is registered on one of the runtime's shards, which completes the read.
#[test]
fn a_thread_that_runs_no_shard_is_served_by_the_runtime() {
    let (a, b) = pair();
    let woke = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&woke));
    let mut cx = Context::from_waker(&waker);
    let mut buf = [0u8; 8];
    assert!(b.src.poll_read(&mut cx, &mut buf, false).is_pending());
    a.io().write_all(b"far").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !woke.0.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the runtime never completed the read"
        );
        std::thread::yield_now();
    }
    match b.src.poll_read(&mut cx, &mut buf, false) {
        Poll::Ready(Ok(3)) => assert_eq!(&buf[..3], b"far"),
        other => panic!("{other:?}"),
    }
}

/// A datagram larger than any buffer the pool may hold cannot be received
/// or sent through it: the operation fails at once, saying what to raise.
#[test]
fn a_datagram_larger_than_the_pool_allows_fails_loudly() {
    let rt = runtime(Some(64 * 1024));
    let (a, b) = UnixDatagram::pair().unwrap();
    let a = Owned::new(a, Kind::UnixDatagram);
    let b = Owned::new(b, Kind::UnixDatagram);
    rt.block_on(async {
        let big = vec![1u8; 100 * 1024];
        // Polled on a worker, whose pool has the cap.
        let (src, bytes) = (Arc::clone(&a.src), big.clone());
        let sent = within(crate::runtime::spawn(async move {
            poll_fn(|cx| src.poll_send(cx, &bytes, super::write::Dest::Connected)).await
        }))
        .await;
        let err = sent.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        assert!(err.to_string().contains("io_buffer_cap"), "{err}");
        let src = Arc::clone(&b.src);
        let got = within(crate::runtime::spawn(async move {
            let mut buf = vec![0u8; 100 * 1024];
            poll_fn(|cx| src.poll_receive(cx, &mut buf, false))
                .await
                .map(|(n, _)| n)
        }))
        .await;
        assert_eq!(got.unwrap_err().kind(), io::ErrorKind::OutOfMemory);
    });
}

#[cfg(target_os = "linux")]
fn set_buffers(fd: std::os::fd::RawFd, bytes: libc::c_int) {
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        // SAFETY: a plain setsockopt with a pointer to our own int.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&bytes as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
    }
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nuclei-source-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

mod owned;
