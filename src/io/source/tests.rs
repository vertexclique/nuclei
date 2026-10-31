//! Sources on real sockets, driven by a shard: the immediate path, the
//! driver's completions, what is kept for later, and the descriptor rules.

use std::future::{Future, poll_fn};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use super::write::Dest;
use super::{Addr, Kind, Source};
use crate::io::raw::Raw;
#[cfg(unix)]
use crate::io::sys;
use crate::runtime::{Builder, Runtime};

/// The connected stream sockets the tests pair: Unix ones where the system
/// has them, else TCP over loopback.
#[cfg(unix)]
type Stream = UnixStream;
#[cfg(windows)]
type Stream = std::net::TcpStream;
#[cfg(unix)]
const STREAM: Kind = Kind::Stream;
#[cfg(windows)]
const STREAM: Kind = Kind::Tcp;

/// The connected datagram sockets the tests pair, likewise.
#[cfg(unix)]
type Datagram = UnixDatagram;
#[cfg(windows)]
type Datagram = std::net::UdpSocket;
#[cfg(unix)]
const DATAGRAM: Kind = Kind::UnixDatagram;
#[cfg(windows)]
const DATAGRAM: Kind = Kind::Udp;

fn stream_pair() -> (Stream, Stream) {
    #[cfg(unix)]
    return UnixStream::pair().unwrap();
    #[cfg(windows)]
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = Stream::connect(listener.local_addr().unwrap()).unwrap();
        let (b, _) = listener.accept().unwrap();
        (a, b)
    }
}

fn datagram_pair() -> (Datagram, Datagram) {
    #[cfg(unix)]
    return UnixDatagram::pair().unwrap();
    #[cfg(windows)]
    {
        let a = Datagram::bind("127.0.0.1:0").unwrap();
        let b = Datagram::bind("127.0.0.1:0").unwrap();
        a.connect(b.local_addr().unwrap()).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        (a, b)
    }
}

/// A test's socket, as a source names it.
trait TestIo {
    fn raw(&self) -> Raw;
}

#[cfg(unix)]
impl<T: AsRawFd> TestIo for T {
    fn raw(&self) -> Raw {
        self.as_raw_fd()
    }
}

#[cfg(windows)]
impl<T: std::os::windows::io::AsRawSocket> TestIo for T {
    fn raw(&self) -> Raw {
        self.as_raw_socket() as Raw
    }
}

/// Readies a socket for a source of its own: non-blocking where the driver
/// makes the calls (a port's operations do not care).
fn adopt(io: &impl TestIo) {
    #[cfg(unix)]
    sys::set_nonblocking(io.raw()).unwrap();
    #[cfg(windows)]
    let _ = io;
}

/// Writes all of `bytes` to the peer from the test, waiting out a full
/// socket buffer.
fn send(mut io: &Stream, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match io.write(bytes) {
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::yield_now(),
            Err(e) => panic!("{e}"),
        }
    }
}

/// A socket with its source, retired before the socket closes: the order a
/// handle keeps.
struct Owned<T: TestIo> {
    src: Arc<Source>,
    io: Option<T>,
}

impl<T: TestIo> Owned<T> {
    fn new(io: T, kind: Kind) -> Self {
        adopt(&io);
        Self {
            src: Source::new(io.raw(), kind),
            io: Some(io),
        }
    }

    fn io(&self) -> &T {
        self.io.as_ref().unwrap()
    }
}

impl<T: TestIo> Drop for Owned<T> {
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

fn pair() -> (Owned<Stream>, Owned<Stream>) {
    let (a, b) = stream_pair();
    (Owned::new(a, STREAM), Owned::new(b, STREAM))
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

/// A test's watchdog: unless dropped within `limit`, it writes `what` and
/// `report()` to the process's standard error (past the harness's capture,
/// which an abort throws away) and aborts, so a hang fails at once and says
/// where it was rather than holding the suite until the job's own limit.
/// Its one user is a unix test.
#[cfg(unix)]
struct Watchdog(Option<std::sync::mpsc::Sender<()>>);

#[cfg(unix)]
impl Watchdog {
    fn new(
        what: &'static str,
        limit: Duration,
        report: impl Fn() -> String + Send + 'static,
    ) -> Self {
        let (done, wait) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = wait.recv_timeout(limit) {
                let _ = writeln!(io::stderr(), "{what} hung: {}", report());
                std::process::abort();
            }
        });
        Self(Some(done))
    }
}

#[cfg(unix)]
impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            let _ = done.send(());
        }
    }
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

/// Fills the kernel's buffers of `io` until it takes no more, even after a
/// pause (a TCP peer's receive window may still grow, as on Windows'
/// loopback, and free room after the first refusal); returns the bytes
/// written.
fn fill(io: &Stream) -> Vec<u8> {
    io.set_nonblocking(true).unwrap();
    let chunk = vec![7u8; 64 * 1024];
    let mut written = Vec::new();
    for round in 0.. {
        let before = written.len();
        loop {
            match (&*io).write(&chunk) {
                Ok(n) => written.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        if round > 0 && written.len() == before {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    written
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
        let peer = a.io().try_clone().unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            send(&peer, b"late");
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
            ) || cfg!(windows) && failed.kind() == io::ErrorKind::ConnectionAborted,
            "{failed:?}"
        );
        assert!(
            poll_fn(|cx| a.src.poll_write(cx, b"x")).await.is_err(),
            "sticky"
        );
        assert!(within(flush(&a.src)).await.is_err(), "the flush reports it");
    });
}

/// Accepts on `listener` while `connect` makes clients: an accept dropped
/// while it waits loses no connection, and a burst is taken whole, part at
/// once and part by the driver. `S` is the accepted sockets' type, of
/// `kind`; `is_peer` checks the address an accept reports.
fn accepts_every_connection<S, C>(
    listener: Owned<impl TestIo>,
    connect: impl Fn() -> C,
    kind: Kind,
    is_peer: fn(&Addr) -> bool,
) where
    S: From<crate::io::raw::Owned> + TestIo,
    C: Read,
{
    let rt = runtime(None);
    rt.block_on(async {
        // An accept that waits, then is dropped: the driver still takes the
        // connection that comes, for the next accept.
        let mut dropped = Box::pin(poll_fn(|cx| listener.src.poll_accept(cx)));
        pends(&mut dropped).await;
        drop(dropped);
        let first = connect();
        crate::time::sleep(Duration::from_millis(10)).await;
        let (conn, addr) = within(poll_fn(|cx| listener.src.poll_accept(cx)))
            .await
            .unwrap();
        assert!(is_peer(&addr), "{addr:?}");
        drop((conn, first));
        // Then a burst: some accepted at once, the rest by the driver.
        let mut waiting = Box::pin(poll_fn(|cx| listener.src.poll_accept(cx)));
        pends(&mut waiting).await;
        let clients: Vec<_> = (0..8).map(|_| connect()).collect();
        let mut accepted = vec![within(waiting).await.unwrap()];
        while accepted.len() < clients.len() {
            accepted.push(
                within(poll_fn(|cx| listener.src.poll_accept(cx)))
                    .await
                    .unwrap(),
            );
        }
        // Each accepted connection is a working peer of one client.
        for (conn, _) in &accepted {
            let conn = Owned::new(S::from(conn.try_clone().unwrap()), kind);
            within(write_all(&conn.src, b"!")).await.unwrap();
        }
        for mut client in clients {
            let mut b = [0u8; 1];
            client.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"!");
        }
    });
}

#[cfg(unix)]
#[test]
fn unix_connections_are_accepted_now_and_by_the_driver_and_none_are_lost() {
    let dir = tempdir();
    let path = dir.join("accept.sock");
    let listener = Owned::new(UnixListener::bind(&path).unwrap(), Kind::UnixListener);
    accepts_every_connection::<UnixStream, _>(
        listener,
        || {
            let client = UnixStream::connect(&path).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client
        },
        Kind::Stream,
        |addr| matches!(addr, Addr::Unix(_)),
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn tcp_connections_are_accepted_now_and_by_the_driver_and_none_are_lost() {
    let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = bound.local_addr().unwrap();
    let listener = Owned::new(bound, Kind::TcpListener);
    accepts_every_connection::<std::net::TcpStream, _>(
        listener,
        || {
            let client = std::net::TcpStream::connect(at).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            client
        },
        Kind::Tcp,
        |addr| matches!(addr, Addr::Inet(peer) if peer.ip().is_loopback()),
    );
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
    let (a, b) = datagram_pair();
    let a = Owned::new(a, DATAGRAM);
    let b = Owned::new(b, DATAGRAM);
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
#[cfg(unix)]
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
    let (a1, b1) = stream_pair();
    let (a2, b2) = stream_pair();
    adopt(&b1);
    adopt(&b2);
    let src = Source::new(b1.raw(), STREAM);
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
    src.revive(b2.raw());
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
    b.src.revive(b.io().raw());
    rt.block_on(async {
        let mut buf = [0u8; 8];
        let n = within(read(&b.src, &mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"ahead");
    });
}

/// A pool given the least cap holds one small buffer beside the receive
/// group: reads still complete, all through either, as each comes back.
#[test]
fn reads_complete_through_a_pool_of_one_small_buffer() {
    let rt = runtime(Some(crate::rt::shard::least_buffer_cap()));
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
        let peer = a.io().try_clone().unwrap();
        let writer = std::thread::spawn(move || send(&peer, &data));
        assert!(within(reader).await);
        writer.join().unwrap();
        drop(a);
    });
}

/// A source first used inside `block_on` on a thread outside the runtime
/// registers with that thread's shard, which runs only while it is inside
/// `block_on`. Once that returns the source goes from it, and a worker's
/// task that polls it next is served.
#[test]
fn a_source_used_in_block_on_is_served_by_a_worker_after_it_returns() {
    let rt = runtime(None);
    let (a, b) = pair();
    rt.block_on(async {
        // A read that waits: the source registers with this thread's shard.
        let mut buf = [0u8; 1];
        let mut reading = Box::pin(read(&b.src, &mut buf));
        pends(&mut reading).await;
        a.io().write_all(b"x").unwrap();
        assert_eq!(within(reading).await.unwrap(), 1);
    });
    let src = Arc::clone(&b.src);
    let (waiting, waits) = std::sync::mpsc::channel();
    let (done, got) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let mut buf = [0u8; 4];
        let mut reading = Box::pin(read(&src, &mut buf));
        pends(&mut reading).await;
        waiting.send(()).unwrap();
        let n = reading.await.unwrap();
        done.send(buf[..n].to_vec()).unwrap();
    })
    .detach();
    waits
        .recv_timeout(Duration::from_secs(5))
        .expect("the worker's read never ran");
    a.io().write_all(b"yz").unwrap();
    let bytes = got
        .recv_timeout(Duration::from_secs(5))
        .expect("the worker's read was never served");
    assert_eq!(bytes, b"yz");
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
    // A poll that finds the side held by the shard is pending, and its holder
    // wakes it again as it lets go.
    loop {
        while !woke.0.swap(false, Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "the runtime never completed the read"
            );
            std::thread::yield_now();
        }
        match b.src.poll_read(&mut cx, &mut buf, false) {
            Poll::Ready(Ok(3)) => {
                assert_eq!(&buf[..3], b"far");
                return;
            }
            Poll::Pending => {}
            other => panic!("{other:?}"),
        }
    }
}

/// A datagram larger than any buffer the pool may hold goes straight from
/// the task's buffer to the kernel and from the kernel into the task's: sent
/// and received whole, a look at readability first taking none of it.
#[cfg(unix)]
#[test]
fn a_datagram_larger_than_the_pool_allows_goes_whole_between_the_tasks_buffers() {
    let group = crate::rt::shard::least_buffer_cap() - crate::buf::CLASSES[0];
    let rt = runtime(Some(group + 64 * 1024));
    let (a, b) = datagram_pair();
    // macOS refuses a datagram larger than the sender's buffer (2 KiB for a Unix socket by
    // default) and holds none larger than the receiver's.
    for (fd, option) in [
        (a.as_raw_fd(), libc::SO_SNDBUF),
        (b.as_raw_fd(), libc::SO_RCVBUF),
    ] {
        let size: libc::c_int = 256 * 1024;
        // SAFETY: a plain setsockopt with a pointer to our own int.
        let set = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                (&size as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(set, 0, "{}", io::Error::last_os_error());
    }
    let a = Owned::new(a, DATAGRAM);
    let b = Owned::new(b, DATAGRAM);
    rt.block_on(async {
        let big = pattern(100 * 1024);
        // Polled on a worker, whose pool has the cap.
        let (src, bytes) = (Arc::clone(&a.src), big.clone());
        let sent = within(crate::runtime::spawn(async move {
            poll_fn(|cx| src.poll_send(cx, &bytes, super::write::Dest::Connected)).await
        }))
        .await;
        assert_eq!(sent.unwrap(), big.len());
        let src = Arc::clone(&b.src);
        let got = within(crate::runtime::spawn(async move {
            poll_fn(|cx| src.poll_read_ready(cx)).await.unwrap();
            let mut buf = vec![0u8; 200 * 1024];
            let n = poll_fn(|cx| src.poll_receive(cx, &mut buf, false))
                .await
                .map(|(n, _)| n)
                .unwrap();
            buf.truncate(n);
            buf
        }))
        .await;
        assert!(got == big, "{} bytes came of {}", got.len(), big.len());
    });
}

/// A datagram the kernel refuses fails its own send with the kernel's error,
/// as std's does, and the next send knows nothing of it.
#[test]
fn a_refused_datagram_fails_its_own_send_and_not_the_next() {
    let rt = runtime(None);
    let a = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    a.connect(b.local_addr().unwrap()).unwrap();
    b.connect(a.local_addr().unwrap()).unwrap();
    let too_long = vec![7u8; 65_508];
    let refused = a.send(&too_long).unwrap_err().raw_os_error();
    let a = Owned::new(a, Kind::Udp);
    let b = Owned::new(b, Kind::Udp);
    rt.block_on(async {
        let src = Arc::clone(&a.src);
        let (first, second) = within(crate::runtime::spawn(async move {
            let first =
                poll_fn(|cx| src.poll_send(cx, &too_long, super::write::Dest::Connected)).await;
            let second =
                poll_fn(|cx| src.poll_send(cx, b"next", super::write::Dest::Connected)).await;
            (first, second)
        }))
        .await;
        assert_eq!(first.unwrap_err().raw_os_error(), refused);
        assert_eq!(second.unwrap(), 4);
        let src = Arc::clone(&b.src);
        let got = within(crate::runtime::spawn(async move {
            let mut buf = [0u8; 16];
            let n = poll_fn(|cx| src.poll_receive(cx, &mut buf, false))
                .await
                .map(|(n, _)| n)
                .unwrap();
            buf[..n].to_vec()
        }))
        .await;
        assert_eq!(got, b"next");
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

#[cfg(unix)]
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

mod biased;
mod owned;
mod requests;
