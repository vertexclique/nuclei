//! The owned datagram calls: whole datagrams in and out of the caller's
//! buffer, truncation, and a receive dropped mid-wait.

use super::*;
use crate::io::IoObject;
use crate::net::opt;

/// Socket buffers that hold the largest datagram these tests send: macOS
/// refuses a datagram larger than the send buffer (by default 9216 bytes for
/// UDP and 2048 for a Unix socket) and a Unix socket's default 4096-byte
/// receive buffer holds none of the large ones.
fn room_for_large<T: IoObject>(socket: &Handle<T>) {
    for name in [opt::SO_SNDBUF, opt::SO_RCVBUF] {
        opt::set_size(socket.get_ref().raw_io(), opt::SOL_SOCKET, name, 256 << 10).unwrap();
    }
}

async fn udp_pair() -> (Handle<UdpSocket>, Handle<UdpSocket>) {
    let a = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
    let b = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
    room_for_large(&a);
    room_for_large(&b);
    (a, b)
}

#[test]
fn udp_owned_datagrams_arrive_whole() {
    let rt = runtime();
    rt.block_on(async {
        let (a, b) = udp_pair().await;
        let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        for len in [0usize, 1, 17, 1200, 9000, 60_000] {
            let data = pattern(len);
            let recv = b.recv_from_owned(Vec::with_capacity(65_536));
            let send = async {
                crate::time::sleep(Duration::from_millis(2)).await;
                let (sent, back) = a.send_to_owned(data.clone(), b_addr).await;
                assert_eq!(sent.unwrap(), len);
                back
            };
            let ((got, buf), back) = within(join(recv, send)).await;
            assert_eq!(back, data);
            let (n, from) = got.unwrap();
            assert_eq!((n, from), (len, a_addr));
            assert_eq!(buf, data);
        }
    });
}

#[test]
fn udp_owned_receive_appends_and_cuts_to_the_spare_capacity() {
    let rt = runtime();
    rt.block_on(async {
        let (a, b) = udp_pair().await;
        let b_addr = b.local_addr().unwrap();
        let (sent, _) = within(a.send_to_owned(pattern(100), b_addr)).await;
        assert_eq!(sent.unwrap(), 100);
        let mut buf = Vec::with_capacity(8 + 40);
        buf.extend_from_slice(b"prefix!!");
        let (got, buf) = within(b.recv_from_owned(buf)).await;
        assert_eq!(got.unwrap().0, 40);
        assert_eq!(&buf[..8], b"prefix!!");
        assert_eq!(&buf[8..], &pattern(100)[..40]);
        // The cut part is gone: the next receive waits for the next datagram.
        let (sent, _) = within(a.send_to_owned(b"next".to_vec(), b_addr)).await;
        assert_eq!(sent.unwrap(), 4);
        let (got, buf) = within(b.recv_owned(Vec::with_capacity(16))).await;
        assert_eq!((got.unwrap(), buf.as_slice()), (4, &b"next"[..]));
    });
}

#[test]
fn udp_owned_connected_calls_talk_to_the_peer() {
    let rt = runtime();
    rt.block_on(async {
        let b = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
        let c = within(Handle::<UdpSocket>::connect(b.local_addr().unwrap()))
            .await
            .unwrap();
        let (sent, buf) = within(c.send_owned(b"ping".to_vec())).await;
        assert_eq!((sent.unwrap(), buf.as_slice()), (4, &b"ping"[..]));
        let (got, buf) = within(b.recv_from_owned(Vec::with_capacity(16))).await;
        let (n, from) = got.unwrap();
        assert_eq!((&buf[..n], from), (&b"ping"[..], c.local_addr().unwrap()));
        within(b.send_to_owned(b"pong".to_vec(), from))
            .await
            .0
            .unwrap();
        let (got, buf) = within(c.recv_owned(Vec::with_capacity(16))).await;
        assert_eq!((got.unwrap(), buf.as_slice()), (4, &b"pong"[..]));
    });
}

#[test]
fn a_dropped_udp_owned_receive_loses_no_datagram() {
    let rt = runtime();
    rt.block_on(async {
        let (a, b) = udp_pair().await;
        let b_addr = b.local_addr().unwrap();
        {
            let mut waiting = Box::pin(b.recv_from_owned(Vec::with_capacity(64)));
            let polled = poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx))).await;
            assert!(polled.is_pending());
        }
        within(a.send_to_owned(b"kept".to_vec(), b_addr))
            .await
            .0
            .unwrap();
        // The datagram may have landed in the dropped future's buffer: the
        // socket keeps it for the next receive.
        let (got, buf) = within(b.recv_owned(Vec::with_capacity(64))).await;
        assert_eq!((got.unwrap(), buf.as_slice()), (4, &b"kept"[..]));
    });
}

#[test]
fn udp_owned_send_to_a_bad_address_gives_the_buffer_back() {
    let rt = runtime();
    rt.block_on(async {
        let (a, _b) = udp_pair().await;
        let (sent, buf) = within(a.send_to_owned(b"x".to_vec(), "not an address")).await;
        assert!(sent.is_err());
        assert_eq!(buf, b"x");
    });
}

#[cfg(unix)]
#[test]
fn unix_datagram_owned_calls_arrive_whole() {
    let rt = runtime();
    rt.block_on(async {
        let dir = tempdir("dgram-owned");
        let (pa, pb) = (dir.join("a"), dir.join("b"));
        let a = Handle::<UnixDatagram>::bind(&pa).unwrap();
        let b = Handle::<UnixDatagram>::bind(&pb).unwrap();
        room_for_large(&a);
        room_for_large(&b);
        // The largest as large as the system sends (FreeBSD's 8 KiB).
        let largest = 60_000.min(crate::alone::largest_unix_datagram());
        for len in [0usize, 5, 3000, largest] {
            let data = pattern(len);
            let recv = b.recv_from_owned(Vec::with_capacity(65_536));
            let send = async {
                crate::time::sleep(Duration::from_millis(2)).await;
                assert_eq!(a.send_to_owned(data.clone(), &pb).await.0.unwrap(), len);
            };
            let ((got, buf), ()) = within(join(recv, send)).await;
            let (n, from) = got.unwrap();
            assert_eq!(n, len);
            assert_eq!(from.as_pathname(), Some(pa.as_path()));
            assert_eq!(buf, data);
        }
        // Truncated to the spare capacity.
        within(a.send_to_owned(pattern(64), &pb)).await.0.unwrap();
        let (got, buf) = within(b.recv_owned(Vec::with_capacity(10))).await;
        assert_eq!((got.unwrap(), buf), (10, pattern(64)[..10].to_vec()));
        // Connected calls on a pair.
        let (x, y) = Handle::<UnixDatagram>::pair().unwrap();
        within(x.send_owned(b"pair".to_vec())).await.0.unwrap();
        let (got, buf) = within(y.recv_owned(Vec::with_capacity(8))).await;
        assert_eq!((got.unwrap(), buf.as_slice()), (4, &b"pair"[..]));
        // A dropped receive loses nothing.
        {
            let mut waiting = Box::pin(y.recv_owned(Vec::with_capacity(8)));
            let polled = poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx))).await;
            assert!(polled.is_pending());
        }
        within(x.send_owned(b"late".to_vec())).await.0.unwrap();
        let (got, buf) = within(y.recv_owned(Vec::with_capacity(8))).await;
        assert_eq!((got.unwrap(), buf.as_slice()), (4, &b"late"[..]));
        std::fs::remove_dir_all(&dir).unwrap();
    });
}

/// The datagrams the process dropped because an owned receive given up had them cut.
fn cut_dropped() -> u64 {
    let stats = crate::stats();
    stats.off_shard_errors.cut_datagrams_dropped
        + stats
            .shards
            .iter()
            .map(|s| s.errors.cut_datagrams_dropped)
            .sum::<u64>()
}

/// Whether the driver receives a datagram into the buffer an owned receive lent it. A ring's UDP
/// side has its task receive it itself, or receives it ahead whole into the ring's group, so a
/// receive given up leaves it whole for the next.
fn driver_fills_lent() -> bool {
    crate::proactor::Proactor::backend() != crate::IoBackend::IoUring
}

/// A waker that only says it was woken.
struct Flag(std::sync::atomic::AtomicBool);

impl std::task::Wake for Flag {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// An owned receive into 16 bytes, given up once the side woke it for a datagram of `len`: the
/// driver received the datagram into its buffer, or (on a ring) the socket said it holds one.
/// Then a datagram of 4 bytes. What the next receive gets: the first datagram whole if it fit or
/// stayed on the socket, else the second, the first dropped and counted.
async fn given_up_then_received(
    a: &Handle<UdpSocket>,
    b: &Handle<UdpSocket>,
    len: usize,
) -> Vec<u8> {
    let b_addr = b.local_addr().unwrap();
    let flag = std::sync::Arc::new(Flag(std::sync::atomic::AtomicBool::new(false)));
    let waker = std::task::Waker::from(std::sync::Arc::clone(&flag));
    let mut waiting = Box::pin(b.recv_from_owned(Vec::with_capacity(16)));
    let first = waiting
        .as_mut()
        .poll(&mut std::task::Context::from_waker(&waker));
    assert!(first.is_pending());
    within(a.send_to(&pattern(len), b_addr)).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !flag.0.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the side never woke the receive"
        );
        crate::time::sleep(Duration::from_millis(1)).await;
    }
    drop(waiting);
    within(a.send_to(b"next", b_addr)).await.unwrap();
    let mut got = vec![0u8; 64 * 1024];
    let (n, _) = within(b.recv_from(&mut got)).await.unwrap();
    got.truncate(n);
    got
}

#[test]
fn a_given_up_owned_receive_drops_a_cut_datagram_and_hands_on_a_whole_one() {
    let rt = runtime();
    rt.block_on(async {
        let (a, b) = udp_pair().await;
        let before = cut_dropped();
        // A datagram that fills the buffer exactly came whole: the next receive takes it.
        assert_eq!(given_up_then_received(&a, &b, 16).await, pattern(16));
        assert_eq!(within(b.recv_from(&mut [0u8; 64])).await.unwrap().0, 4);
        assert_eq!(cut_dropped(), before);
        // One longer than the buffer was cut: dropped and counted, and the next receive takes
        // the one after it.
        // Where the task receives it itself, it stays whole for the next.
        let got = given_up_then_received(&a, &b, 100).await;
        if driver_fills_lent() {
            assert_eq!(got, b"next");
            assert_eq!(cut_dropped(), before + 1);
        } else {
            assert_eq!(got, pattern(100));
            assert_eq!(within(b.recv_from(&mut [0u8; 64])).await.unwrap().0, 4);
            assert_eq!(cut_dropped(), before);
        }
    });
}
