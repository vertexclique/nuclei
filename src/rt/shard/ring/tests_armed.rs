//! A socket whose task receives its datagrams itself waits on one poll that
//! stays armed: every wait after the first submits nothing, each datagram
//! that comes wakes the task, and none is lost or cut.

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
