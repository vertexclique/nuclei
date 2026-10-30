//! Multishot receives on a shard's ring: one receive serves every arrival,
//! the bytes come whole and in order across the group's buffers, a reader
//! that falls behind is stopped at its cap, a dry group hands reads to the
//! pool, a lent buffer takes over from a receive, and a chunk outlives its
//! shard.

use core::future::poll_fn;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::io::source::{Kind, Source};
use crate::io::sys;
use crate::rt::backend::Backend;
use crate::rt::context;
use crate::rt::shard::{Limits, Shard};
use crate::syscore::linux::iouring::ring::RingConfig;

/// A shard on a ring with a receive group of `buffers` of `size` bytes, or
/// `None` where the kernel gives no ring.
fn recv_shard(buffers: u16, size: u32) -> Option<Shard> {
    ring_shard_of(64, buffers, size)
}

/// As [`recv_shard`], with a group of whole buffers: a datagram cannot share
/// a buffer filled in pieces, so multishot datagrams need this one.
fn plain_shard(buffers: u16, size: u32) -> Option<Shard> {
    super::super::recv::INCREMENTAL_FOR_TEST.with(|t| t.set(Some(false)));
    let shard = ring_shard_of(64, buffers, size);
    super::super::recv::INCREMENTAL_FOR_TEST.with(|t| t.set(None));
    shard
}

/// As [`recv_shard`], on a ring of `entries` submission entries (twice as
/// many completions): a ring whose completion queue overflows ends its
/// multishot operations itself.
pub(super) fn ring_shard_of(entries: u32, buffers: u16, size: u32) -> Option<Shard> {
    let shard = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: Backend::Ring(RingConfig {
            recv_buffers: buffers,
            recv_buffer_size: size,
            ..RingConfig::new(entries)
        }),
    });
    match shard {
        Ok(shard) if shard.ring().is_some_and(|r| r.recv_group().is_some()) => Some(shard),
        Ok(_) => {
            eprintln!("skipping: the ring has no receive group here");
            None
        }
        Err(err) => {
            eprintln!("skipping: io_uring unavailable: {err}");
            None
        }
    }
}

/// A socket of a pair with its source, and the pair's other end.
pub(super) fn source_pair() -> (UnixStream, Arc<Source>, UnixStream) {
    let (mine, peer) = UnixStream::pair().unwrap();
    sys::set_nonblocking(mine.as_raw_fd()).unwrap();
    let src = Source::new(mine.as_raw_fd(), Kind::Stream);
    (mine, src, peer)
}

/// Sets both buffers of a socket to `bytes`.
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

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

async fn read(src: &Arc<Source>, buf: &mut [u8]) -> usize {
    poll_fn(|cx| src.poll_read(cx, buf, false)).await.unwrap()
}

async fn read_exact(src: &Arc<Source>, out: &mut [u8]) {
    let mut at = 0;
    while at < out.len() {
        match read(src, &mut out[at..]).await {
            0 => panic!("the stream ended after {at} bytes"),
            n => at += n,
        }
    }
}

/// Lets the shard run for `d`.
async fn pause(d: Duration) {
    crate::time::sleep(d).await;
}

/// Messages that come one at a time are all served by the one receive the
/// first read armed.
#[test]
fn one_multishot_receive_serves_every_arrival() {
    let Some(shard) = recv_shard(16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = source_pair();
    // Each message goes once the one before was read: each is an arrival
    // of its own, however slow the reader.
    let (read_one, wait_read) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        for i in 0..32u8 {
            peer.write_all(&[i; 100]).unwrap();
            wait_read.recv().unwrap();
        }
        peer
    });
    shard.block_on(async {
        for i in 0..32u8 {
            let mut message = [0u8; 100];
            read_exact(&src, &mut message).await;
            assert!(message.iter().all(|&b| b == i), "message {i} changed");
            read_one.send(()).unwrap();
        }
    });
    let (armed, arrived, _) = shard.ring().unwrap().multishot_counts();
    assert_eq!(armed, 1, "the receive was armed again {} times", armed - 1);
    assert!(arrived >= 32, "only {arrived} arrivals for 32 messages");
    drop(writer.join().unwrap());
    src.retire();
}

/// A stream far larger than the group comes whole and in order, its
/// buffers given back and filled again as the reader takes them (and
/// through the pool when a burst outruns them).
#[test]
fn a_long_stream_arrives_whole_and_in_order_across_the_buffers() {
    let Some(shard) = recv_shard(16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = source_pair();
    let data = pattern(2 << 20);
    let expected = data.clone();
    let writer = std::thread::spawn(move || {
        peer.write_all(&data).unwrap();
        peer
    });
    shard.block_on(async {
        // Reads under the size a buffer is read into directly, so the
        // group serves them.
        let mut got = vec![0u8; expected.len()];
        for piece in got.chunks_mut(4096) {
            read_exact(&src, piece).await;
        }
        assert!(got == expected, "the bytes arrived changed or out of order");
    });
    // Some reads may go through the pool when a burst outruns the group.
    let (_, arrived, _) = shard.ring().unwrap().multishot_counts();
    assert!(arrived > 0, "no byte came through the group");
    drop(writer.join().unwrap());
    // Every buffer came back once the reader took its bytes.
    assert_eq!(shard.ring().unwrap().held(), 0);
    src.retire();
}

/// A reader that stops reading while bytes keep coming: its receive is
/// stopped once it holds past its cap, so it takes nothing more from the
/// group however many bytes come after, and once it reads again every byte
/// comes, in order. The writer sends a little past the cap, waits for the
/// reader to see the receive stopped, then sends a few pieces more: what
/// the stop bounds is what comes after it, whatever the batches the kernel
/// made before. The parts are sized to the cap, not to the socket's
/// buffers, which a system may keep small: the receive takes the first
/// part up to its stop, and the rest waits in the socket.
#[test]
fn a_reader_that_falls_behind_is_stopped_at_its_cap_and_loses_nothing() {
    let Some(shard) = recv_shard(256, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (mine, src, mut peer) = source_pair();
    set_buffers(mine.as_raw_fd(), 8 << 20);
    set_buffers(peer.as_raw_fd(), 8 << 20);
    let ring = shard.ring().unwrap();
    let cap = ring.recv_cap().unwrap();
    const PIECE: usize = 16 * 1024;
    // Past the cap by four pieces, then four more after the stop.
    let first_pieces = cap / PIECE + 4;
    let data = pattern(PIECE * (first_pieces + 4));
    let expected = data.clone();
    let (first_sent, wait_first) = std::sync::mpsc::channel::<()>();
    let (go_on, wait_go_on) = std::sync::mpsc::channel::<()>();
    let (all_sent, wait_all) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        let (first, rest) = data.split_at(PIECE * first_pieces);
        for piece in first.chunks(PIECE) {
            peer.write_all(piece).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        first_sent.send(()).unwrap();
        wait_go_on.recv().unwrap();
        peer.write_all(rest).unwrap();
        all_sent.send(()).unwrap();
        peer
    });
    shard.block_on(async {
        // One read arms the receive, then the reader falls behind.
        let mut first = vec![0u8; 1];
        read_exact(&src, &mut first).await;
        let deadline = Instant::now() + Duration::from_secs(20);
        while wait_first.try_recv().is_err() {
            assert!(Instant::now() < deadline, "the first part was never sent");
            pause(Duration::from_millis(5)).await;
        }
        // Until what the receive holds stops growing: it stopped.
        let mut held = ring.held();
        loop {
            pause(Duration::from_millis(20)).await;
            let now = ring.held();
            if now == held {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the receive never stopped: {now} bytes"
            );
            held = now;
        }
        assert!(
            held > cap,
            "it stopped at {held} bytes, under its cap of {cap}"
        );
        go_on.send(()).unwrap();
        while wait_all.try_recv().is_err() {
            assert!(Instant::now() < deadline, "the rest was never sent");
            pause(Duration::from_millis(5)).await;
        }
        pause(Duration::from_millis(50)).await;
        assert_eq!(ring.held(), held, "the stopped receive took more");
        let mut rest = vec![0u8; expected.len() - 1];
        read_exact(&src, &mut rest).await;
        assert_eq!(first[0], expected[0]);
        assert!(
            rest == expected[1..],
            "bytes changed or were lost after the stop"
        );
    });
    drop(writer.join().unwrap());
    assert_eq!(ring.held(), 0, "buffers were not given back");
    src.retire();
}

/// A group of two buffers: arrivals outrun it, its receives end dry, and
/// the reads go on through the pool, every byte in order.
#[test]
fn a_dry_group_hands_reads_to_the_pool_and_loses_nothing() {
    let Some(shard) = recv_shard(2, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (mine, src, mut peer) = source_pair();
    set_buffers(mine.as_raw_fd(), 4 << 20);
    set_buffers(peer.as_raw_fd(), 4 << 20);
    let data = pattern(3 << 20);
    let expected = data.clone();
    let writer = std::thread::spawn(move || {
        peer.write_all(&data).unwrap();
        peer
    });
    shard.block_on(async {
        // Let the backlog build, so the receive finds more than the group.
        let mut first = vec![0u8; 1];
        read_exact(&src, &mut first).await;
        pause(Duration::from_millis(100)).await;
        let mut rest = vec![0u8; expected.len() - 1];
        read_exact(&src, &mut rest).await;
        assert!(
            first[0] == expected[0] && rest == expected[1..],
            "the bytes arrived changed or out of order"
        );
    });
    let (_, _, dry) = shard.ring().unwrap().multishot_counts();
    assert!(
        dry > 0,
        "the group never ran dry, so the pool path was not taken"
    );
    drop(writer.join().unwrap());
    src.retire();
}

/// A lent buffer whose read waits with nothing read ahead takes the next
/// bytes itself: the armed receive is stopped for it, and those bytes come
/// as no arrival of the group.
#[test]
fn a_lent_read_stops_the_receive_and_takes_the_bytes_itself() {
    let Some(shard) = recv_shard(16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = source_pair();
    let ring = shard.ring().unwrap();
    shard.block_on(async {
        // A plain read arms the receive.
        peer.write_all(b"plain").unwrap();
        let mut plain = [0u8; 5];
        read_exact(&src, &mut plain).await;
        assert_eq!(&plain, b"plain");
        let (_, before, _) = ring.multishot_counts();
        // A lent read that waits: the receive is stopped for it.
        let mut lent = Box::pin(src.read_owned(Vec::with_capacity(64)));
        let polled = poll_fn(|cx| core::task::Poll::Ready(lent.as_mut().poll(cx))).await;
        assert!(polled.is_pending());
        let deadline = Instant::now() + Duration::from_secs(5);
        while ring.held() > 0 || ring_has_receive(ring) {
            assert!(Instant::now() < deadline, "the receive was never stopped");
            pause(Duration::from_millis(1)).await;
        }
        peer.write_all(b"lent").unwrap();
        let (n, buf) = lent.await;
        assert_eq!((n.unwrap(), &buf[..]), (4, &b"lent"[..]));
        let (_, after, _) = ring.multishot_counts();
        assert_eq!(
            after, before,
            "the lent bytes came as an arrival of the group"
        );
    });
    src.retire();
}

/// Whether a multishot receive is in flight on `ring`.
fn ring_has_receive(ring: &crate::rt::shard::ring::ShardRing) -> bool {
    ring.receives_in_flight() > 0
}

/// Bytes a receive handed out are still there after their shard (and its
/// group) went: the chunks keep the buffers mapped.
#[test]
fn a_chunk_outlives_its_shard() {
    let Some(shard) = recv_shard(16, 4096) else {
        return;
    };
    let (_mine, src, mut peer) = source_pair();
    let data = pattern(40 * 1024);
    {
        let _current = context::enter(&shard);
        let expected = data.clone();
        shard.block_on(async {
            peer.write_all(&data).unwrap();
            // One byte read: the rest waits in the read-ahead, in chunks.
            let mut first = [0u8; 1];
            read_exact(&src, &mut first).await;
            assert_eq!(first[0], expected[0]);
            pause(Duration::from_millis(50)).await;
        });
    }
    drop(shard);
    // Read on a thread with no shard: what is held comes from the chunks.
    let mut rest = vec![0u8; 16 * 1024];
    let woke = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waker = std::task::Waker::from(Arc::new(Flag(Arc::clone(&woke))));
    let mut cx = std::task::Context::from_waker(&waker);
    match src.poll_read(&mut cx, &mut rest, false) {
        core::task::Poll::Ready(Ok(n)) => {
            assert!(n > 0);
            assert!(
                rest[..n] == data[1..=n],
                "the bytes held changed after the shard went"
            );
        }
        other => panic!("the bytes held were not there: {other:?}"),
    }
    src.retire();
}

struct Flag(Arc<std::sync::atomic::AtomicBool>);

impl std::task::Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A TCP listener with its source, and its address.
fn listener_source() -> (std::net::TcpListener, Arc<Source>, std::net::SocketAddr) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    sys::set_nonblocking(listener.as_raw_fd()).unwrap();
    let src = Source::new(listener.as_raw_fd(), Kind::TcpListener);
    (listener, src, addr)
}

/// The next connection; a process short of descriptors (other tests run in
/// parallel) is waited out.
async fn accept_stream(src: &Arc<Source>) -> crate::io::raw::Owned {
    loop {
        match poll_fn(|cx| src.poll_accept_stream(cx)).await {
            Ok(conn) => return conn,
            Err(err) if crate::rt::counters::out_of_descriptors(&err) => {
                crate::time::sleep(Duration::from_millis(1)).await;
            }
            Err(err) => panic!("{err}"),
        }
    }
}

/// A client connection, made again while the process has no descriptor for
/// it: a client that died of that would leave the accepting side waiting for
/// good.
fn connect_patiently(addr: std::net::SocketAddr) -> std::net::TcpStream {
    loop {
        match std::net::TcpStream::connect(addr) {
            Ok(c) => return c,
            Err(err) if crate::rt::counters::out_of_descriptors(&err) => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(err) => panic!("{err}"),
        }
    }
}

/// Connections taken without their addresses are all served by the one
/// multishot accept the first armed, or by one armed again after the process
/// had no descriptor to accept into (other tests run in parallel), which ends
/// the accept.
#[test]
fn one_multishot_accept_serves_a_stream_of_connections() {
    let Some(shard) = recv_shard(16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_listener, src, addr) = listener_source();
    let clients = std::thread::spawn(move || {
        (0..16)
            .map(|i| {
                let mut c = connect_patiently(addr);
                c.write_all(&[i]).unwrap();
                std::thread::sleep(Duration::from_millis(1));
                c
            })
            .collect::<Vec<_>>()
    });
    let mut seen = Vec::new();
    let mut ended = 0;
    let served = shard.block_on(crate::time::timeout(Duration::from_secs(30), async {
        for _ in 0..16 {
            let conn = loop {
                match poll_fn(|cx| src.poll_accept_stream(cx)).await {
                    Ok(conn) => break std::net::TcpStream::from(conn),
                    Err(err) if crate::rt::counters::out_of_descriptors(&err) => {
                        ended += 1;
                        crate::time::sleep(Duration::from_millis(1)).await;
                    }
                    Err(err) => panic!("{err}"),
                }
            };
            // An accepted connection is non-blocking; its byte may still be
            // on its way.
            conn.set_nonblocking(false).unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut b = [0u8; 1];
            let n = std::io::Read::read(&mut &conn, &mut b).unwrap();
            assert_eq!(n, 1);
            seen.push(b[0]);
        }
    }));
    served.expect("a connection never came");
    seen.sort_unstable();
    assert_eq!(
        seen,
        (0..16).collect::<Vec<u8>>(),
        "a connection was lost or taken twice"
    );
    let (armed, _, _) = shard.ring().unwrap().multishot_counts();
    assert_eq!(
        armed,
        1 + ended,
        "the accept was armed again {} times, {ended} of them after the process had no descriptor",
        armed - 1
    );
    drop(clients.join().unwrap());
    src.retire();
}

/// An accept that wants the peer's address, taking a connection the
/// multishot accept brought, gets it from the kernel.
#[test]
fn an_accept_after_a_multishot_one_gets_its_peer_from_the_kernel() {
    let Some(shard) = recv_shard(16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_listener, src, addr) = listener_source();
    shard.block_on(async {
        // A stream accept arms the multishot accept.
        let first = std::net::TcpStream::connect(addr).unwrap();
        drop(accept_stream(&src).await);
        let second = std::net::TcpStream::connect(addr).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let (conn, peer) = loop {
            let mut accepted = Box::pin(poll_fn(|cx| src.poll_accept(cx)));
            match crate::time::timeout(Duration::from_millis(50), &mut accepted).await {
                Ok(done) => break done.unwrap(),
                Err(_) => assert!(Instant::now() < deadline, "the connection never came"),
            }
        };
        match peer {
            crate::io::source::Addr::Inet(peer) => assert_eq!(
                peer,
                second.local_addr().unwrap(),
                "the peer's address is not the client's"
            ),
            other => panic!("a TCP peer came as {other:?}"),
        }
        drop((conn, first, second));
    });
    src.retire();
}

/// Connections that come while no task takes them: past its cap the accept
/// is stopped, none is lost, and once they are taken the accept is armed
/// again.
#[test]
fn past_its_cap_a_multishot_accept_is_stopped_and_loses_no_connection() {
    // A completion queue with room for every connection: only the cap stops
    // the accept.
    let Some(shard) = ring_shard_of(1024, 16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Room in the backlog for every client.
    // SAFETY: a plain listen on our own socket.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1024) }, 0);
    sys::set_nonblocking(listener.as_raw_fd()).unwrap();
    let src = Source::new(listener.as_raw_fd(), Kind::TcpListener);
    let ring = shard.ring().unwrap();
    const CLIENTS: usize = 300;
    shard.block_on(async {
        let first = std::net::TcpStream::connect(addr).unwrap();
        drop(accept_stream(&src).await);
        let clients: Vec<_> = (0..CLIENTS)
            .map(|_| std::net::TcpStream::connect(addr).unwrap())
            .collect();
        // Nobody takes them: the accept is stopped once past its cap.
        let deadline = Instant::now() + Duration::from_secs(5);
        while ring.receives_in_flight() > 0 {
            assert!(Instant::now() < deadline, "the accept was never stopped");
            pause(Duration::from_millis(5)).await;
        }
        for _ in 0..CLIENTS {
            drop(accept_stream(&src).await);
        }
        // One more, which only a new accept takes.
        let late = std::net::TcpStream::connect(addr).unwrap();
        let mut taken = Box::pin(accept_stream(&src));
        assert!(
            crate::time::timeout(Duration::from_secs(5), &mut taken)
                .await
                .is_ok(),
            "no accept was armed again after the stop"
        );
        drop((first, clients, late));
    });
    let (armed, _, _) = ring.multishot_counts();
    assert!(armed >= 2, "the accept was not armed again after its stop");
    src.retire();
}

/// A bound UDP socket with its source.
fn udp_source() -> (std::net::UdpSocket, Arc<Source>, std::net::SocketAddr) {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    sys::set_nonblocking(socket.as_raw_fd()).unwrap();
    // Room for every datagram the tests send at once.
    set_buffers(socket.as_raw_fd(), 4 << 20);
    let src = Source::new(socket.as_raw_fd(), Kind::Udp);
    (socket, src, addr)
}

async fn receive(src: &Arc<Source>, out: &mut [u8]) -> (usize, crate::io::source::Addr) {
    poll_fn(|cx| src.poll_receive(cx, out, false))
        .await
        .unwrap()
}

/// A group whose buffers hold any datagram.
const DATAGRAM_BUFFERS: u32 = 66 * 1024;

/// Datagrams from two senders, up to 60000 bytes, all come whole and with
/// their senders through the one multishot receive the first armed.
#[test]
fn datagrams_come_whole_with_their_senders_through_one_multishot_receive() {
    let Some(shard) = plain_shard(16, DATAGRAM_BUFFERS) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_socket, src, addr) = udp_source();
    let senders = [
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap(),
    ];
    let sizes = [1usize, 100, 1500, 9000, 60_000, 33];
    let mut expected = Vec::new();
    shard.block_on(async {
        // Armed by the first receive.
        senders[0].send_to(b"first", addr).unwrap();
        let mut buf = vec![0u8; 65536];
        let (n, _) = receive(&src, &mut buf).await;
        assert_eq!(&buf[..n], b"first");
        for (i, size) in sizes.iter().enumerate() {
            for sender in &senders {
                let datagram: Vec<u8> = (0..*size).map(|j| (i * 31 + j) as u8).collect();
                sender.send_to(&datagram, addr).unwrap();
                expected.push((sender.local_addr().unwrap(), datagram));
            }
        }
        for (from, datagram) in &expected {
            let (n, got_from) = receive(&src, &mut buf).await;
            assert_eq!(n, datagram.len(), "a datagram was cut");
            assert!(
                buf[..n] == datagram[..],
                "a datagram changed or came out of order"
            );
            match got_from {
                crate::io::source::Addr::Inet(got) => assert_eq!(got, *from),
                other => panic!("a UDP sender came as {other:?}"),
            }
        }
    });
    let (armed, arrived, _) = shard.ring().unwrap().multishot_counts();
    assert_eq!(armed, 1, "the receive was armed again {} times", armed - 1);
    assert!(arrived >= expected.len(), "only {arrived} arrivals");
    src.retire();
}

/// A datagram longer than the reader's buffer is cut to it, as the kernel
/// cuts it, and the next one comes whole.
#[test]
fn a_multishot_datagram_is_cut_to_the_readers_buffer() {
    let Some(shard) = plain_shard(16, DATAGRAM_BUFFERS) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_socket, src, addr) = udp_source();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    shard.block_on(async {
        sender.send_to(&[7u8; 1000], addr).unwrap();
        sender.send_to(b"next", addr).unwrap();
        let mut small = [0u8; 100];
        let (n, _) = receive(&src, &mut small).await;
        assert_eq!((n, small), (100, [7u8; 100]));
        let mut buf = [0u8; 16];
        let (n, _) = receive(&src, &mut buf).await;
        assert_eq!(&buf[..n], b"next");
    });
    src.retire();
}

/// A group whose buffers cannot hold every datagram: UDP receives stay
/// single, so no datagram is ever cut at a buffer's end.
#[test]
fn datagrams_wider_than_the_groups_buffers_never_go_multishot() {
    let Some(shard) = recv_shard(16, 16 * 1024) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_socket, src, addr) = udp_source();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    shard.block_on(async {
        let big = vec![3u8; 60_000];
        sender.send_to(&big, addr).unwrap();
        let mut buf = vec![0u8; 65536];
        let (n, _) = receive(&src, &mut buf).await;
        assert_eq!(n, big.len(), "the datagram was cut");
    });
    let (armed, _, _) = shard.ring().unwrap().multishot_counts();
    assert_eq!(
        armed, 0,
        "a datagram receive went multishot into buffers too small"
    );
    src.retire();
}
