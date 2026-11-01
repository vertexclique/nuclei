//! A socket whose task receives its datagrams itself (a Unix datagram
//! socket's, and a UDP one's on a ring whose group cannot hold any datagram
//! whole) waits on one poll that stays armed: every wait after the first
//! submits nothing, each datagram that comes wakes the task, and none is
//! lost or cut.

use core::future::poll_fn;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;

use crate::io::source::{Kind, Source};
use crate::io::sys;
use crate::rt::context;

/// Datagrams that come one at a time, each after the one before was
/// received, are all waited for on the one poll the first wait armed.
#[test]
fn one_armed_poll_serves_every_wait_of_a_unix_datagram() {
    let Some(shard) = super::tests_recv::ring_shard_of(64, 16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (mine, peer) = UnixDatagram::pair().unwrap();
    sys::set_nonblocking(mine.as_raw_fd()).unwrap();
    let src = Source::new(mine.as_raw_fd(), Kind::UnixDatagram);
    let (taken, wait_taken) = std::sync::mpsc::channel::<()>();
    let sender = std::thread::spawn(move || {
        for i in 0..32u8 {
            peer.send(&[i; 200]).unwrap();
            wait_taken.recv().unwrap();
        }
        peer
    });
    shard.block_on(async {
        for i in 0..32u8 {
            let mut buf = [0u8; 512];
            let (n, _) = poll_fn(|cx| src.poll_receive(cx, &mut buf, false))
                .await
                .unwrap();
            assert_eq!(n, 200, "datagram {i} was cut");
            assert!(buf[..n].iter().all(|&b| b == i), "datagram {i} changed");
            taken.send(()).unwrap();
        }
    });
    // None when every datagram was there before its receive asked.
    let armed = shard.ring().unwrap().polls_armed();
    assert!(armed <= 1, "the poll was armed again {} times", armed - 1);
    drop(sender.join().unwrap());
    src.retire();
}

/// As for a Unix datagram, a UDP socket on a ring whose group fills buffers
/// in pieces, or holds less than the largest datagram, receives itself and
/// waits on one armed poll, with no multishot receive of the ring's going out.
#[test]
fn one_armed_poll_serves_every_wait_of_a_udp_socket_the_ring_receives_none_ahead() {
    let Some(shard) = super::tests_recv::ring_shard_of(64, 16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    assert!(!shard.ring().unwrap().datagrams_ahead());
    let mine = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let to = mine.local_addr().unwrap();
    sys::set_nonblocking(mine.as_raw_fd()).unwrap();
    let src = Source::new(mine.as_raw_fd(), Kind::Udp);
    let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let from = peer.local_addr().unwrap();
    let (taken, wait_taken) = std::sync::mpsc::channel::<()>();
    let sender = std::thread::spawn(move || {
        for i in 0..32u8 {
            peer.send_to(&[i; 1200], to).unwrap();
            wait_taken.recv().unwrap();
        }
    });
    shard.block_on(async {
        for i in 0..32u8 {
            let mut buf = [0u8; 2048];
            let (n, sender) = poll_fn(|cx| src.poll_receive(cx, &mut buf, false))
                .await
                .unwrap();
            assert_eq!(n, 1200, "datagram {i} was cut");
            assert!(buf[..n].iter().all(|&b| b == i), "datagram {i} changed");
            match sender {
                crate::io::source::Addr::Inet(sender) => assert_eq!(sender, from),
                other => panic!("a UDP sender came as {other:?}"),
            }
            taken.send(()).unwrap();
        }
    });
    let ring = shard.ring().unwrap();
    assert!(
        ring.polls_armed() <= 1,
        "the poll was armed again {} times",
        ring.polls_armed() - 1
    );
    assert_eq!(
        ring.multishot_counts().0,
        0,
        "a multishot receive of the ring's went out"
    );
    sender.join().unwrap();
    src.retire();
}
