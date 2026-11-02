//! A stream read by readiness on a shard's ring: a reader that keeps up reads straight from the
//! socket on the ring's multishot poll and arms no receive, one whose bytes run ahead switches to
//! the multishot receive, and one whose arrivals then fit its reads goes by readiness again, every
//! byte in order throughout (proofs/tla/ring/RingPoll.tla).

use core::future::poll_fn;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use super::tests_recv::ring_shard_of;
use crate::io::source::{Kind, Source};
use crate::io::sys;
use crate::rt::context;

/// A TCP socket of a connected pair with its source, reading as any stream does, and the pair's
/// other end: TCP, whose kernel counts what a read leaves on every kernel a ring runs on (a Unix
/// stream's counts from Linux 6.17).
fn ready_pair() -> (TcpStream, Arc<Source>, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mine, _) = listener.accept().unwrap();
    mine.set_nodelay(true).unwrap();
    peer.set_nodelay(true).unwrap();
    sys::set_nonblocking(mine.as_raw_fd()).unwrap();
    let src = Source::new(mine.as_raw_fd(), Kind::Tcp);
    (mine, src, peer)
}

async fn read_exact(src: &Arc<Source>, out: &mut [u8]) {
    let mut at = 0;
    while at < out.len() {
        match poll_fn(|cx| src.poll_read(cx, &mut out[at..], false))
            .await
            .unwrap()
        {
            0 => panic!("the stream ended after {at} bytes"),
            n => at += n,
        }
    }
}

/// Messages that come one at a time, each read whole, are read straight from the socket on the
/// ring's poll: no multishot receive is armed, and every message comes whole and in order.
#[test]
fn a_reader_that_keeps_up_reads_by_readiness() {
    let Some(shard) = ring_shard_of(64, 16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = ready_pair();
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
    let ring = shard.ring().unwrap();
    let (armed, _, _) = ring.multishot_counts();
    assert_eq!(armed, 0, "a reader that keeps up armed {armed} receives");
    assert!(ring.polls_armed() >= 1, "no poll was armed");
    drop(writer.join().unwrap());
    src.retire();
}

/// A stream whose bytes run far ahead of small reads switches to the multishot receive, and once
/// its messages come one at a time and each fits a read, goes by readiness again: the receive is
/// armed once, a poll after it, and every byte comes in order.
#[test]
fn bytes_that_run_ahead_switch_to_the_receive_and_back() {
    let Some(shard) = ring_shard_of(64, 16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = ready_pair();
    let burst: Vec<u8> = (0..32 * 1024).map(|i| (i * 7 + i / 251) as u8).collect();
    let expected = burst.clone();
    let (read_one, wait_read) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        peer.write_all(&burst).unwrap();
        wait_read.recv().unwrap();
        for i in 0..16u8 {
            peer.write_all(&[i; 100]).unwrap();
            wait_read.recv().unwrap();
        }
        peer
    });
    let ring = shard.ring().unwrap();
    let polls_before_tail = shard.block_on(async {
        let mut got = vec![0u8; expected.len()];
        for piece in got.chunks_mut(64) {
            read_exact(&src, piece).await;
        }
        assert!(got == expected, "the burst came changed or out of order");
        read_one.send(()).unwrap();
        let polls = ring.polls_armed();
        for i in 0..16u8 {
            let mut message = [0u8; 100];
            read_exact(&src, &mut message).await;
            assert!(message.iter().all(|&b| b == i), "message {i} changed");
            read_one.send(()).unwrap();
        }
        polls
    });
    let (armed, arrived, _) = ring.multishot_counts();
    assert!(armed >= 1, "the burst never switched to the receive");
    assert!(arrived >= 1, "the receive brought nothing");
    assert!(
        ring.polls_armed() > polls_before_tail,
        "the messages that fit their reads never went by readiness again"
    );
    drop(writer.join().unwrap());
    src.retire();
}
