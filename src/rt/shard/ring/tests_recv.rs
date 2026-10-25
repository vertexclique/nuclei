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
    let shard = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: Backend::Ring(RingConfig {
            recv_buffers: buffers,
            recv_buffer_size: size,
            ..RingConfig::new(64)
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
fn source_pair() -> (UnixStream, Arc<Source>, UnixStream) {
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
    let writer = std::thread::spawn(move || {
        for i in 0..32u8 {
            peer.write_all(&[i; 100]).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        peer
    });
    shard.block_on(async {
        let mut got = vec![0u8; 3200];
        read_exact(&src, &mut got).await;
        for (i, message) in got.chunks(100).enumerate() {
            assert!(message.iter().all(|&b| b == i as u8), "message {i} changed");
        }
    });
    let (armed, arrived, _) = shard.ring().unwrap().multishot_counts();
    assert_eq!(armed, 1, "the receive was armed again {} times", armed - 1);
    assert!(arrived >= 16, "only {arrived} arrivals for 32 messages");
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
        let mut got = vec![0u8; expected.len()];
        read_exact(&src, &mut got).await;
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

/// A reader that stops reading while bytes keep coming in small batches:
/// its receive is stopped once it holds past its cap, so it takes nothing
/// more from the group however long the bytes keep coming, and once it
/// reads again every byte comes, in order.
#[test]
fn a_reader_that_falls_behind_is_stopped_at_its_cap_and_loses_nothing() {
    let Some(shard) = recv_shard(256, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (mine, src, mut peer) = source_pair();
    set_buffers(mine.as_raw_fd(), 8 << 20);
    set_buffers(peer.as_raw_fd(), 8 << 20);
    const PIECE: usize = 16 * 1024;
    const PIECES: usize = 80;
    let data = pattern(PIECE * PIECES);
    let expected = data.clone();
    // Small batches: each arrives on its own, well under the group.
    let writer = std::thread::spawn(move || {
        for piece in data.chunks(PIECE) {
            peer.write_all(piece).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        peer
    });
    let ring = shard.ring().unwrap();
    let cap = ring.recv_cap().unwrap();
    shard.block_on(async {
        // One read arms the receive, then the reader falls behind while
        // the writer goes on.
        let mut first = vec![0u8; 1];
        read_exact(&src, &mut first).await;
        pause(Duration::from_millis(PIECES as u64 * 5 + 100)).await;
        // Past the cap the receive stops: at most the batch in which it
        // passed it comes on top.
        let held = ring.held();
        assert!(
            held <= cap + 2 * PIECE,
            "a reader that took nothing holds {held} bytes, past its cap of {cap}"
        );
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
