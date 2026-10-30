//! Zero-copy sends on a shard's ring: an owned write is copied whatever its size; a zero-copy
//! write arrives whole, ends at its first completions, and hands its buffer back only after the
//! kernel's notifications; a dropped `Released` or write leaves the buffer to the notifications;
//! a send that fails still ends; and a shard that goes waits for the notifications.

use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::{Context, Poll};
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::Shard;
use super::TABLE_FOR_TEST;
use crate::io::source::ring::SEND_ZC_MIN;
use crate::io::source::{Kind, Source};
use crate::io::sys;
use crate::net::Released;
use crate::rt::backend::Backend;
use crate::rt::context;
use crate::rt::shard::Limits;
use crate::syscore::linux::iouring::ring::RingConfig;

fn ring_shard() -> Option<Shard> {
    TABLE_FOR_TEST.with(|t| t.set(Some(8)));
    let shard = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: Backend::Ring(RingConfig::new(64)),
    });
    TABLE_FOR_TEST.with(|t| t.set(None));
    match shard {
        Ok(shard) => Some(shard),
        Err(err) => {
            eprintln!("skipping: io_uring unavailable: {err}");
            None
        }
    }
}

fn tcp_source() -> (TcpStream, Arc<Source>, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mine = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    sys::set_nonblocking(mine.as_raw_fd()).unwrap();
    let src = Source::new(mine.as_raw_fd(), Kind::Tcp);
    (mine, src, peer)
}

fn zc_available(shard: &Shard) -> bool {
    let has = shard.ring().is_some_and(|r| r.zc_stream());
    if !has {
        eprintln!("skipping: the kernel has no zero-copy send");
    }
    has
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// A thread that reads `len` bytes from `peer`.
fn reader(mut peer: TcpStream, len: usize) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut got = vec![0u8; len];
        peer.read_exact(&mut got).unwrap();
        got
    })
}

/// Runs the shard until `done` holds, failing with `what` at the deadline.
fn run_until(shard: &Shard, mut done: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(50);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        shard.block_on(crate::time::sleep(pause));
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

/// Waits on the shard for `released`, checking as it resolves that every first completion the
/// ring saw had its notification by then.
fn await_released(shard: &Shard, mut released: Released) -> Vec<u8> {
    let ring = shard.ring().expect("a ring shard");
    shard.block_on(poll_fn(|cx| {
        let polled = Pin::new(&mut released).poll(cx);
        if polled.is_ready() {
            let (firsts, notifs) = ring.zc_seen();
            assert_eq!(firsts, notifs, "the buffer came back before a notification");
        }
        polled
    }))
}

fn poll_once<F: Future + Unpin>(fut: &mut F) -> Poll<F::Output> {
    Pin::new(fut).poll(&mut Context::from_waker(std::task::Waker::noop()))
}

#[test]
fn a_large_owned_write_is_copied_and_arrives_whole() {
    let Some(shard) = ring_shard() else { return };
    let _current = context::enter(&shard);
    let (_mine, src, peer) = tcp_source();
    let data = pattern(5 * SEND_ZC_MIN + 13);
    let read = reader(peer, data.len());
    let (result, back) = shard.block_on(src.write_owned(data.clone()));
    assert_eq!(result.unwrap(), data.len());
    assert!(back == data);
    assert!(read.join().unwrap() == data, "the bytes changed on the way");
    assert_eq!(shard.ring().expect("a ring").zc_seen(), (0, 0));
}

#[test]
fn a_zero_copy_write_arrives_whole_and_its_buffer_comes_back_after_the_notifications() {
    let Some(shard) = ring_shard() else { return };
    if !zc_available(&shard) {
        return;
    }
    let _current = context::enter(&shard);
    let ring = shard.ring().expect("a ring shard");
    for len in [SEND_ZC_MIN * 7 / 2, SEND_ZC_MIN] {
        let (_mine, src, peer) = tcp_source();
        let data = pattern(len);
        let at = data.as_ptr();
        let read = reader(peer, len);
        let before = ring.zc_seen().0;
        let (result, released) = shard.block_on(src.write_zero_copy(data));
        assert_eq!(result.unwrap(), len);
        assert!(
            ring.zc_seen().0 > before,
            "the write never went out zero-copy"
        );
        let back = await_released(&shard, released);
        assert_eq!(back.as_ptr(), at, "another buffer came back");
        assert!(back == pattern(len), "the buffer changed");
        assert!(read.join().unwrap() == back, "the bytes changed on the way");
    }
}

#[test]
fn a_zero_copy_write_below_the_threshold_is_copied_and_ready_at_once() {
    let Some(shard) = ring_shard() else { return };
    let _current = context::enter(&shard);
    let (_mine, src, peer) = tcp_source();
    let data = pattern(SEND_ZC_MIN - 1);
    let at = data.as_ptr();
    let read = reader(peer, data.len());
    let (result, mut released) = shard.block_on(src.write_zero_copy(data));
    assert_eq!(result.unwrap(), SEND_ZC_MIN - 1);
    let Poll::Ready(back) = poll_once(&mut released) else {
        panic!("the copied write's buffer did not come back at once");
    };
    assert_eq!(back.as_ptr(), at);
    assert!(back == pattern(SEND_ZC_MIN - 1));
    assert!(read.join().unwrap() == back);
    assert_eq!(shard.ring().expect("a ring").zc_seen(), (0, 0));
}

/// The peer reads only after the write ended: the write does not wait for it, and the buffer
/// comes back only after the notifications, which wait for what the peer's window did not take.
#[test]
fn a_zero_copy_write_ends_before_the_peer_reads() {
    let Some(shard) = ring_shard() else { return };
    if !zc_available(&shard) {
        return;
    }
    let _current = context::enter(&shard);
    let (_mine, src, peer) = tcp_source();
    let data = pattern(SEND_ZC_MIN);
    let (result, mut released) = shard.block_on(src.write_zero_copy(data.clone()));
    assert_eq!(result.unwrap(), data.len());
    let ring = shard.ring().expect("a ring shard");
    let (firsts, notifs) = ring.zc_seen();
    assert!(firsts > 0, "the write never went out zero-copy");
    if poll_once(&mut released).is_ready() {
        assert_eq!(firsts, notifs, "the buffer came back before a notification");
    }
    let read = reader(peer, data.len());
    let back = await_released(&shard, released);
    assert!(back == data);
    assert!(read.join().unwrap() == data);
}

#[test]
fn a_dropped_released_leaves_the_buffer_to_the_notifications() {
    let Some(shard) = ring_shard() else { return };
    if !zc_available(&shard) {
        return;
    }
    let _current = context::enter(&shard);
    let ring = shard.ring().expect("a ring shard");
    let (_mine, src, peer) = tcp_source();
    let data = pattern(3 * SEND_ZC_MIN);
    let read = reader(peer, data.len());
    let (result, released) = shard.block_on(src.write_zero_copy(data.clone()));
    assert_eq!(result.unwrap(), data.len());
    drop(released);
    assert!(read.join().unwrap() == data, "the bytes changed on the way");
    run_until(
        &shard,
        || ring.in_flight() == 0,
        "a send still holds the buffer after the peer read it all",
    );
    let (firsts, notifs) = ring.zc_seen();
    assert!(firsts > 0, "the write never went out zero-copy");
    assert_eq!(firsts, notifs, "a notification is missing");
}

/// Dropped with part of it out, the write is finished, and a later write goes after it: the
/// stream gets the buffer whole, then the rest. One dropped before any of it went is withdrawn.
#[test]
fn a_dropped_zero_copy_write_is_whole_or_absent_and_later_writes_follow() {
    let Some(shard) = ring_shard() else { return };
    if !zc_available(&shard) {
        return;
    }
    let _current = context::enter(&shard);
    let ring = shard.ring().expect("a ring shard");
    let (mine, src, peer) = tcp_source();
    // More than the socket takes while nobody reads.
    let data = pattern(16 * SEND_ZC_MIN);
    let mut write = Box::pin(src.write_zero_copy(data.clone()));
    shard.block_on(async {
        let polled = poll_fn(|cx| Poll::Ready(write.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "the socket took all of it unread");
    });
    run_until(
        &shard,
        || ring.zc_seen().0 > 0,
        "the first chunk never completed",
    );
    drop(write);
    let read = std::thread::spawn(move || {
        let mut got = Vec::new();
        (&peer).read_to_end(&mut got).unwrap();
        got
    });
    let (wrote, _) = shard.block_on(src.write_owned(b"tail".to_vec()));
    assert_eq!(wrote.unwrap(), 4);
    shard.block_on(poll_fn(|cx| src.poll_flush(cx))).unwrap();
    mine.shutdown(std::net::Shutdown::Write).unwrap();
    let got = read.join().unwrap();
    let mut whole = data;
    whole.extend_from_slice(b"tail");
    assert!(
        got == whole || got == b"tail",
        "the stream got part of the dropped write ({} bytes)",
        got.len()
    );
    run_until(
        &shard,
        || ring.in_flight() == 0,
        "a send still holds the dropped write's buffer",
    );
    let (firsts, notifs) = ring.zc_seen();
    assert_eq!(firsts, notifs, "a notification is missing");
}

/// Writes one after the other without waiting for their buffers: the bytes arrive in order, and
/// every buffer comes back whole.
#[test]
fn zero_copy_writes_pipeline_and_every_buffer_comes_back() {
    let Some(shard) = ring_shard() else { return };
    if !zc_available(&shard) {
        return;
    }
    let _current = context::enter(&shard);
    let (_mine, src, peer) = tcp_source();
    let bufs: Vec<Vec<u8>> = (0..16)
        .map(|i| {
            let mut buf = pattern(SEND_ZC_MIN + i);
            buf.rotate_left(i * 31);
            buf
        })
        .collect();
    let read = reader(peer, bufs.iter().map(Vec::len).sum());
    let mut released = Vec::new();
    for buf in &bufs {
        let (result, back) = shard.block_on(src.write_zero_copy(buf.clone()));
        assert_eq!(result.unwrap(), buf.len());
        released.push(back);
    }
    assert!(
        read.join().unwrap() == bufs.concat(),
        "the writes arrived out of order"
    );
    for (buf, back) in bufs.iter().zip(released) {
        assert!(shard.block_on(back) == *buf);
    }
    let (firsts, notifs) = shard.ring().expect("a ring shard").zc_seen();
    assert!(firsts >= bufs.len(), "a write never went out zero-copy");
    assert_eq!(firsts, notifs, "a buffer came back before its notification");
}

#[test]
fn a_zero_copy_write_that_fails_gives_the_buffer_back_and_leaves_nothing_out() {
    let Some(shard) = ring_shard() else { return };
    if !zc_available(&shard) {
        return;
    }
    let _current = context::enter(&shard);
    let (mine, src, peer) = tcp_source();
    drop(peer);
    // A reset connection fails the send when it runs.
    let data = pattern(SEND_ZC_MIN * 2);
    let mut last = None;
    for _ in 0..4 {
        let (result, released) = shard.block_on(src.write_zero_copy(data.clone()));
        let back = await_released(&shard, released);
        assert!(back == data, "the buffer must come back whole");
        if let Err(err) = result {
            last = Some(err);
            break;
        }
    }
    assert!(last.is_some(), "writing to a closed peer never failed");
    let ring = shard.ring().expect("a ring shard");
    let (firsts, notifs) = ring.zc_seen();
    assert!(firsts > 0, "the writes never went out zero-copy");
    assert_eq!(firsts, notifs, "a notification is missing");
    drop(mine);
    src.retire();
    shard.block_on(async {});
    assert_eq!(ring.in_flight(), 0, "a send is still out");
}

/// A shard that goes with a send waiting for its notification waits for it, so the buffer comes
/// back rather than being kept for good.
#[test]
fn a_shard_that_goes_waits_for_the_notifications() {
    let Some(shard) = ring_shard() else { return };
    if !zc_available(&shard) {
        return;
    }
    let (_mine, src, peer) = tcp_source();
    let data = pattern(SEND_ZC_MIN);
    let before = super::leaked();
    let mut released = {
        let _current = context::enter(&shard);
        let (result, released) = shard.block_on(src.write_zero_copy(data.clone()));
        assert_eq!(result.unwrap(), data.len());
        released
    };
    let read = reader(peer, data.len());
    drop(shard);
    assert_eq!(super::leaked(), before, "a send was left with its buffer");
    let Poll::Ready(back) = poll_once(&mut released) else {
        panic!("the buffer did not come back as the shard went");
    };
    assert!(back == data);
    assert!(read.join().unwrap() == data);
}

#[test]
fn released_is_send_and_unpin() {
    fn send_unpin<T: Send + Unpin + 'static>() {}
    send_unpin::<Released>();
}
