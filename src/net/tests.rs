//! The socket handles through their own API, on a runtime: what each call
//! returns, the trait impls on a handle and on a shared reference, and the
//! rules that keep the descriptor the handle's.

use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::Poll;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::time::Duration;

use futures_core::Stream;
use futures_io::{AsyncRead, AsyncWrite};

use crate::io::Handle;
use crate::runtime::{Builder, Runtime, spawn};

fn runtime() -> Runtime {
    Builder::new()
        .worker_threads(2)
        .pin_workers(false)
        .build()
        .unwrap()
}

async fn within<F: Future>(fut: F) -> F::Output {
    crate::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("the operation never completed")
}

async fn read_some<R: AsyncRead + Unpin>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    poll_fn(|cx| Pin::new(&mut *r).poll_read(cx, buf)).await
}

async fn read_exact<R: AsyncRead + Unpin>(r: &mut R, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match read_some(r, buf).await? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => buf = &mut buf[n..],
        }
    }
    Ok(())
}

async fn write_all<W: AsyncWrite + Unpin>(w: &mut W, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = poll_fn(|cx| Pin::new(&mut *w).poll_write(cx, buf)).await?;
        buf = &buf[n..];
    }
    poll_fn(|cx| Pin::new(&mut *w).poll_flush(cx)).await
}

async fn close<W: AsyncWrite + Unpin>(w: &mut W) -> io::Result<()> {
    poll_fn(|cx| Pin::new(&mut *w).poll_close(cx)).await
}

async fn next<S: Stream + Unpin>(s: &mut S) -> Option<S::Item> {
    poll_fn(|cx| Pin::new(&mut *s).poll_next(cx)).await
}

/// A minimal executor of a thread that runs no shard: park until woken.
fn plain_block_on<F: Future>(fut: F) -> F::Output {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Wake, Waker};
    struct Unpark(std::thread::Thread, AtomicBool);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.1.store(true, Ordering::SeqCst);
            self.0.unpark();
        }
    }
    let unpark = Arc::new(Unpark(std::thread::current(), AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&unpark));
    let mut cx = Context::from_waker(&waker);
    let mut fut = core::pin::pin!(fut);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
        while !unpark.1.swap(false, Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "never woken");
            std::thread::park_timeout(Duration::from_millis(100));
        }
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 13 + i / 241) as u8).collect()
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nuclei-net-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_tcp_echo_carries_a_large_message_both_ways() {
    let rt = runtime();
    rt.block_on(async {
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = spawn(async move {
            let (mut conn, peer) = within(listener.accept()).await.unwrap();
            assert!(peer.ip().is_loopback());
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = within(read_some(&mut conn, &mut buf)).await.unwrap();
                if n == 0 {
                    break;
                }
                within(write_all(&mut conn, &buf[..n])).await.unwrap();
            }
            close(&mut conn).await.unwrap();
        });
        let mut client = within(Handle::<TcpStream>::connect(addr)).await.unwrap();
        let data = pattern(3 << 20);
        let (reader, writer) = (&client, &client);
        let expected = data.clone();
        let mut r = reader;
        let read_back = async move {
            let mut got = vec![0u8; expected.len()];
            read_exact(&mut r, &mut got).await.unwrap();
            got == expected
        };
        let mut w = writer;
        let send = async move {
            write_all(&mut w, &data).await.unwrap();
            close(&mut w).await.unwrap();
        };
        // Both halves at once, through shared references.
        let (same, ()) = within(join(read_back, send)).await;
        assert!(same, "the echo came back changed");
        within(server).await;
        let mut rest = [0u8; 1];
        assert_eq!(within(read_some(&mut client, &mut rest)).await.unwrap(), 0);
    });
}

/// Runs two futures together on the calling task.
async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (Box::pin(a), Box::pin(b));
    let (mut ra, mut rb) = (None, None);
    poll_fn(|cx| {
        if ra.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(cx)
        {
            ra = Some(v);
        }
        if rb.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(cx)
        {
            rb = Some(v);
        }
        if ra.is_some() && rb.is_some() {
            Poll::Ready((ra.take().unwrap(), rb.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

#[test]
fn send_recv_and_peek_on_tcp() {
    let rt = runtime();
    rt.block_on(async {
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = spawn(async move { Handle::<TcpStream>::connect(addr).await.unwrap() });
        let (conn, _) = within(listener.accept()).await.unwrap();
        let client = within(client).await;
        assert_eq!(within(client.send(b"ping")).await.unwrap(), 4);
        let mut buf = [0u8; 8];
        assert_eq!(within(conn.peek(&mut buf)).await.unwrap(), 4);
        assert_eq!(within(conn.recv(&mut buf)).await.unwrap(), 4);
        assert_eq!(&buf[..4], b"ping");
    });
}

#[test]
fn connect_tries_each_address_and_reports_the_last_error() {
    let rt = runtime();
    rt.block_on(async {
        // A port nobody listens on: bound and closed again.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let err = within(Handle::<TcpStream>::connect(closed))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap();
        let both: &[SocketAddr] = &[closed, open];
        let stream = within(Handle::<TcpStream>::connect(both)).await.unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open);
        let none: &[SocketAddr] = &[];
        let err = within(Handle::<TcpStream>::connect(none))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    });
}

#[test]
fn incoming_and_accept_multi_yield_every_connection() {
    let rt = runtime();
    rt.block_on(async {
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let clients: Vec<_> = (0..6).map(|_| TcpStream::connect(addr).unwrap()).collect();
        let mut incoming = listener.incoming();
        for _ in 0..3 {
            within(next(&mut incoming)).await.unwrap().unwrap();
        }
        drop(incoming);
        let mut multi = within(listener.accept_multi()).await.unwrap();
        // The generator owns its own copy of the listener.
        drop(listener);
        for _ in 0..3 {
            let conn = within(next(&mut multi)).await.unwrap();
            assert!(conn.peer_addr().is_ok());
        }
        assert!(multi.take_error().is_none());
        drop(clients);
    });
}

#[test]
fn udp_sends_receives_peeks_and_connects() {
    let rt = runtime();
    rt.block_on(async {
        let a = Handle::<UdpSocket>::bind("127.0.0.1:0").unwrap();
        let b = Handle::<UdpSocket>::bind("127.0.0.1:0").unwrap();
        let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        let mut buf = [0u8; 64];
        // A receive that waits, completed by the driver.
        let sender = async {
            crate::time::sleep(Duration::from_millis(5)).await;
            a.send_to(b"hello", b_addr).await.unwrap()
        };
        let (got, sent) = within(join(b.peek_from(&mut buf), sender)).await;
        let (n, from) = got.unwrap();
        assert_eq!((n, from, sent), (5, a_addr, 5));
        let (n, from) = within(b.recv_from(&mut buf)).await.unwrap();
        assert_eq!((&buf[..n], from), (&b"hello"[..], a_addr));
        // A connected socket talks to its peer only.
        let c = within(Handle::<UdpSocket>::connect(b_addr)).await.unwrap();
        assert_eq!(within(c.send(b"conn")).await.unwrap(), 4);
        let (n, from) = within(b.recv_from(&mut buf)).await.unwrap();
        assert_eq!((&buf[..n], from), (&b"conn"[..], c.local_addr().unwrap()));
        within(b.send_to(b"back", c.local_addr().unwrap()))
            .await
            .unwrap();
        assert_eq!(within(c.peek(&mut buf)).await.unwrap(), 4);
        assert_eq!(within(c.recv(&mut buf)).await.unwrap(), 4);
        assert_eq!(&buf[..4], b"back");
    });
}

#[test]
fn unix_streams_connect_accept_and_pair() {
    let rt = runtime();
    let dir = tempdir("stream");
    let path = dir.join("s.sock");
    rt.block_on(async {
        let listener = Handle::<UnixListener>::bind(&path).unwrap();
        let p = path.clone();
        let client = spawn(async move {
            let mut c = Handle::<UnixStream>::connect(&p).await.unwrap();
            write_all(&mut c, b"over unix").await.unwrap();
            c
        });
        let mut incoming = listener.incoming();
        let mut conn = within(next(&mut incoming)).await.unwrap().unwrap();
        let mut buf = [0u8; 9];
        within(read_exact(&mut conn, &mut buf)).await.unwrap();
        assert_eq!(&buf, b"over unix");
        drop(within(client).await);
        let (a, b) = Handle::<UnixStream>::pair().unwrap();
        assert_eq!(within(a.send(b"x")).await.unwrap(), 1);
        assert_eq!(within(b.peek(&mut buf)).await.unwrap(), 1);
        assert_eq!(within(b.recv(&mut buf)).await.unwrap(), 1);
        let (accepted, _) = {
            let p = path.clone();
            let c = spawn(async move { Handle::<UnixStream>::connect(&p).await.unwrap() });
            let accepted = within(listener.accept()).await.unwrap();
            drop(within(c).await);
            accepted
        };
        drop(accepted);
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unix_datagrams_send_to_paths_and_pairs() {
    let rt = runtime();
    let dir = tempdir("dgram");
    let (pa, pb) = (dir.join("a.sock"), dir.join("b.sock"));
    rt.block_on(async {
        let a = Handle::<UnixDatagram>::bind(&pa).unwrap();
        let b = Handle::<UnixDatagram>::bind(&pb).unwrap();
        assert_eq!(within(a.send_to(b"to b", &pb)).await.unwrap(), 4);
        let mut buf = [0u8; 16];
        let (n, from) = within(b.peek_from(&mut buf)).await.unwrap();
        assert_eq!(n, 4);
        assert_eq!(from.as_pathname(), Some(pa.as_path()));
        let (n, _) = within(b.recv_from(&mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"to b");
        let (x, y) = Handle::<UnixDatagram>::pair().unwrap();
        assert_eq!(within(x.send(b"pair")).await.unwrap(), 4);
        assert_eq!(within(y.peek(&mut buf)).await.unwrap(), 4);
        assert_eq!(within(y.recv(&mut buf)).await.unwrap(), 4);
        // An unnamed sender.
        let (n, from) = {
            within(x.send(b"anon")).await.unwrap();
            within(y.recv_from(&mut buf)).await.unwrap()
        };
        assert_eq!(n, 4);
        assert!(from.is_unnamed());
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_handle_made_by_new_finds_out_what_it_wraps() {
    let rt = runtime();
    rt.block_on(async {
        let std_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = std_listener.local_addr().unwrap();
        let listener = Handle::new(std_listener).unwrap();
        let client = Handle::new(TcpStream::connect(addr).unwrap()).unwrap();
        let (conn, _) = within(listener.accept()).await.unwrap();
        within(client.send(b"new")).await.unwrap();
        let mut buf = [0u8; 3];
        assert_eq!(within(conn.recv(&mut buf)).await.unwrap(), 3);
        let (a, b) = UnixDatagram::pair().unwrap();
        let (a, b) = (Handle::new(a).unwrap(), Handle::new(b).unwrap());
        within(a.send(b"dg")).await.unwrap();
        assert_eq!(within(b.recv(&mut buf)).await.unwrap(), 2);
    });
}

/// `get_mut` hands the object out; swapping it for another makes the next
/// operation use the new one, and nothing the old one left shows up.
#[test]
fn a_get_mut_swap_is_seen_by_the_next_operation() {
    let rt = runtime();
    rt.block_on(async {
        let (old_peer, mine) = UnixStream::pair().unwrap();
        let (new_peer, other) = UnixStream::pair().unwrap();
        let mut handle = Handle::new(mine).unwrap();
        let mut buf = [0u8; 8];
        // A receive that waits, then is dropped, and the old peer's bytes
        // that the driver reads ahead for it.
        {
            let mut waiting = Box::pin(handle.recv(&mut buf));
            let polled = poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx))).await;
            assert!(polled.is_pending());
        }
        (&old_peer).write_all(b"old").unwrap();
        crate::time::sleep(Duration::from_millis(10)).await;
        other.set_nonblocking(true).unwrap();
        let old = std::mem::replace(handle.get_mut(), other);
        drop(old);
        (&new_peer).write_all(b"new").unwrap();
        let n = within(handle.recv(&mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"new");
    });
}

/// The handle's number changes hands while the old file lives on through a
/// dup: the old file's bytes never show up on the new socket, and the new
/// socket's bytes are all its own.
#[test]
fn a_dup_and_close_never_lets_the_driver_read_a_reused_number() {
    let rt = runtime();
    rt.block_on(async {
        let (old_peer, mine) = UnixStream::pair().unwrap();
        let keep = mine.try_clone().unwrap();
        let handle = Handle::new(mine).unwrap();
        let mut buf = [0u8; 8];
        {
            let mut waiting = Box::pin(handle.recv(&mut buf));
            let polled = poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx))).await;
            assert!(polled.is_pending());
        }
        // The handle goes; its number is taken over at once by another socket.
        let number = handle.into_inner().into_raw_fd();
        let (new_peer, other) = UnixStream::pair().unwrap();
        // SAFETY: `number` is this test's own descriptor; dup2 replaces it.
        assert_eq!(unsafe { libc::dup2(other.as_raw_fd(), number) }, number);
        drop(other);
        // SAFETY: `number` is this test's own descriptor.
        let reused = UnixStream::from(unsafe { OwnedFd::from_raw_fd(number) });
        let reused = Handle::new(reused).unwrap();
        (&old_peer).write_all(b"old").unwrap();
        (&new_peer).write_all(b"mine").unwrap();
        let n = within(reused.recv(&mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"mine");
        keep.set_nonblocking(true).unwrap();
        assert_eq!((&keep).read(&mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"old");
    });
}

/// A handle dropped while an operation waits: the operation's future goes
/// with it, the descriptor closes after the registration, and the peer sees
/// the end.
#[test]
fn a_handle_dropped_with_an_operation_waiting_closes_cleanly() {
    let rt = runtime();
    rt.block_on(async {
        let (a, b) = Handle::<UnixStream>::pair().unwrap();
        let mut buf = [0u8; 8];
        {
            let mut waiting = Box::pin(a.recv(&mut buf));
            let polled = poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx))).await;
            assert!(polled.is_pending());
        }
        drop(a);
        let mut b = b;
        assert_eq!(within(read_some(&mut b, &mut buf)).await.unwrap(), 0);
    });
}

#[test]
fn a_handle_used_from_another_executor_works() {
    // Polled by a minimal executor on a plain thread: a runtime shard
    // serves the source.
    let (a, b) = Handle::<UnixStream>::pair().unwrap();
    let echo = std::thread::spawn(move || {
        let mut buf = [0u8; 5];
        plain_block_on(async {
            let mut b = b;
            read_exact(&mut b, &mut buf).await.unwrap();
            write_all(&mut b, &buf).await.unwrap();
        });
        buf
    });
    let got = plain_block_on(async {
        let mut a = a;
        write_all(&mut a, b"plain").await.unwrap();
        let mut buf = [0u8; 5];
        read_exact(&mut a, &mut buf).await.unwrap();
        buf
    });
    assert_eq!(&got, b"plain");
    assert_eq!(&echo.join().unwrap(), b"plain");
}

#[allow(dead_code)]
fn send_and_sync_follow_the_object() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Handle<TcpStream>>();
    send_sync::<Handle<UdpSocket>>();
    send_sync::<Handle<UnixStream>>();
    fn send<T: Send>() {}
    send::<super::TcpStreamGenerator>();
}

#[allow(dead_code)]
fn stream_impls_are_on_the_handle_and_its_reference(h: &Handle<TcpStream>, u: &Handle<UnixStream>) {
    fn io<T: AsyncRead + AsyncWrite>(_: T) {}
    io(h);
    io(u);
}

/// Writes until a write goes behind (the socket is full); returns every
/// byte written, in order.
async fn fill_behind(h: &Handle<UnixStream>) -> Vec<u8> {
    let mut sent = Vec::new();
    let mut seq = 0usize;
    while !h.source.has_unsent() {
        let chunk: Vec<u8> = (0..16 * 1024)
            .map(|i| ((seq * 16 * 1024 + i) % 251) as u8)
            .collect();
        seq += 1;
        let n = within(h.send(&chunk)).await.unwrap();
        sent.extend_from_slice(&chunk[..n]);
    }
    sent
}

fn read_to_end_blocking(mut peer: UnixStream) -> Vec<u8> {
    peer.set_nonblocking(false).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut got = Vec::new();
    peer.read_to_end(&mut got).unwrap();
    got
}

#[test]
fn a_handle_dropped_with_bytes_behind_still_delivers_them_then_closes() {
    let rt = runtime();
    let (mine, peer) = UnixStream::pair().unwrap();
    let sent = rt.block_on(async {
        let handle = Handle::new(mine).unwrap();
        let sent = fill_behind(&handle).await;
        drop(handle);
        sent
    });
    let got = std::thread::spawn(move || read_to_end_blocking(peer))
        .join()
        .unwrap();
    assert_eq!(got.len(), sent.len(), "bytes accepted were lost");
    assert!(got == sent, "the bytes arrived changed or out of order");
}

#[test]
fn into_inner_hands_the_object_back_after_the_bytes_behind() {
    let rt = runtime();
    let (mine, peer) = UnixStream::pair().unwrap();
    let (sent, reader) = rt.block_on(async {
        let handle = Handle::new(mine).unwrap();
        let mut sent = fill_behind(&handle).await;
        // The socket is full: a reader starts draining it.
        let reader = std::thread::spawn(move || read_to_end_blocking(peer));
        let mut stream = handle.into_inner();
        // Written with std after the handle's bytes: it must come after them.
        stream.set_nonblocking(false).unwrap();
        stream.write_all(b"after").unwrap();
        sent.extend_from_slice(b"after");
        (sent, reader)
    });
    let got = reader.join().unwrap();
    assert!(got == sent, "{} bytes sent, {} got", sent.len(), got.len());
}

#[test]
fn get_mut_hands_the_object_out_after_the_bytes_behind() {
    let rt = runtime();
    let (mine, peer) = UnixStream::pair().unwrap();
    let (sent, reader) = rt.block_on(async {
        let mut handle = Handle::new(mine).unwrap();
        let mut sent = fill_behind(&handle).await;
        let reader = std::thread::spawn(move || read_to_end_blocking(peer));
        // Written with std after the handle's bytes: it must come after them.
        let direct = handle.get_mut();
        direct.set_nonblocking(false).unwrap();
        direct.write_all(b"direct").unwrap();
        direct.set_nonblocking(true).unwrap();
        sent.extend_from_slice(b"direct");
        within(handle.send(b"!")).await.unwrap();
        sent.extend_from_slice(b"!");
        (sent, reader)
    });
    let got = reader.join().unwrap();
    assert!(got == sent, "{} bytes sent, {} got", sent.len(), got.len());
}

#[test]
fn vectored_reads_and_writes_scatter_and_gather_in_order() {
    use std::io::{IoSlice, IoSliceMut};
    let rt = runtime();
    rt.block_on(async {
        let (a, b) = Handle::<UnixStream>::pair().unwrap();
        let (mut a, mut b) = (&a, &b);
        // At once: the socket takes the gathered bytes.
        let n = within(poll_fn(|cx| {
            Pin::new(&mut a).poll_write_vectored(cx, &[IoSlice::new(b"one"), IoSlice::new(b"two")])
        }))
        .await
        .unwrap();
        assert_eq!(n, 6);
        let (mut x, mut y) = ([0u8; 2], [0u8; 8]);
        let n = within(poll_fn(|cx| {
            let mut bufs = [IoSliceMut::new(&mut x), IoSliceMut::new(&mut y)];
            Pin::new(&mut b).poll_read_vectored(cx, &mut bufs)
        }))
        .await
        .unwrap();
        assert_eq!(n, 6);
        assert_eq!((&x, &y[..4]), (b"on", &b"etwo"[..]));
        // A scattered read that waits, completed by the driver.
        let writer = async {
            crate::time::sleep(Duration::from_millis(5)).await;
            write_all(&mut a, b"late bytes").await.unwrap();
        };
        let (mut p, mut q) = ([0u8; 4], [0u8; 16]);
        let reader = poll_fn(|cx| {
            let mut bufs = [IoSliceMut::new(&mut p), IoSliceMut::new(&mut q)];
            Pin::new(&mut b).poll_read_vectored(cx, &mut bufs)
        });
        let (n, ()) = within(join(reader, writer)).await;
        let n = n.unwrap();
        let mut got = p[..n.min(4)].to_vec();
        got.extend_from_slice(&q[..n.saturating_sub(4)]);
        assert_eq!(got, b"late bytes"[..n]);
    });
}
