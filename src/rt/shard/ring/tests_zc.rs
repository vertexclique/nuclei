//! Zero-copy sends on a shard's ring: a large owned write arrives whole, its
//! buffer comes back only after the kernel's notification, and a send that
//! fails on its first completion still ends.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use super::super::Shard;
use super::TABLE_FOR_TEST;
use crate::io::source::ring::SEND_ZC_MIN;
use crate::io::source::{Kind, Source};
use crate::io::sys;
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

#[test]
fn a_large_owned_write_goes_out_zero_copy_and_arrives_whole() {
    let Some(shard) = ring_shard() else { return };
    if !zc_available(&shard) {
        return;
    }
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = tcp_source();
    let data = pattern(5 * 1024 * 1024 + 13);
    let want = data.clone();
    let reader = std::thread::spawn(move || {
        let mut got = vec![0u8; want.len()];
        peer.read_exact(&mut got).unwrap();
        assert!(got == want, "the bytes changed on the way");
    });
    let ring = shard.ring().expect("a ring shard");
    let (result, back) = shard.block_on(src.write_owned(data));
    assert_eq!(result.unwrap(), back.len());
    let (firsts, notifs) = ring.zc_seen();
    assert!(firsts > 0, "the write never went out zero-copy");
    assert_eq!(firsts, notifs, "the buffer came back before a notification");
    reader.join().unwrap();
}

#[test]
fn a_write_below_the_threshold_is_copied() {
    let Some(shard) = ring_shard() else { return };
    if !zc_available(&shard) {
        return;
    }
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = tcp_source();
    let data = pattern(SEND_ZC_MIN - 1);
    let (result, back) = shard.block_on(src.write_owned(data.clone()));
    assert_eq!(result.unwrap(), back.len());
    let mut got = vec![0u8; data.len()];
    peer.read_exact(&mut got).unwrap();
    assert_eq!(got, data);
    assert_eq!(shard.ring().expect("a ring").zc_seen(), (0, 0));
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
        let (result, back) = shard.block_on(src.write_owned(data.clone()));
        assert_eq!(back, data, "the buffer must come back whole");
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
