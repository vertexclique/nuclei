//! The ring's admission cap: a ring shrunk under memory pressure gives its
//! receive group no more than half its completion queue, and receives that
//! wait on the network, multishot or one-shot, do not count against the cap,
//! so a send is never held up behind its own reply's receive or behind idle
//! connections.

use core::future::poll_fn;
use core::pin::pin;
use core::task::Poll;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::tests_recv::{ring_shard_of, source_pair};
use crate::rt::context;

/// How long a send may take; far above what an idle shard needs.
const DEADLINE: Duration = Duration::from_secs(5);

#[test]
fn a_small_ring_gives_its_group_no_more_than_half_its_completion_queue() {
    // 64 entries are 128 completion slots: 64 buffers at most.
    let Some(shard) = ring_shard_of(64, 256, 4096) else {
        return;
    };
    assert_eq!(shard.ring().unwrap().group_buffers(), Some(64));
    let Some(shard) = ring_shard_of(64, 32, 4096) else {
        return;
    };
    assert_eq!(shard.ring().unwrap().group_buffers(), Some(32));
}

#[test]
fn a_request_is_sent_while_the_receive_for_its_reply_is_armed_on_a_small_ring() {
    let Some(shard) = ring_shard_of(64, 256, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = source_pair();
    let server = std::thread::spawn(move || {
        let mut request = [0u8; 1];
        peer.read_exact(&mut request).unwrap();
        peer.write_all(b"r").unwrap();
        peer
    });
    shard.block_on(async {
        let mut reply = [0u8; 1];
        let mut read = pin!(poll_fn(|cx| src.poll_read(cx, &mut reply, false)));
        // The receive for the reply is armed before the request goes.
        poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let sent = crate::time::timeout(DEADLINE, poll_fn(|cx| src.poll_write(cx, b"q"))).await;
        assert!(
            sent.is_ok(),
            "the request waited behind its reply's receive"
        );
        let got = crate::time::timeout(DEADLINE, read).await;
        assert_eq!(got.expect("the reply never came").unwrap(), 1);
    });
    drop(server.join().unwrap());
    src.retire();
}

#[test]
fn idle_connections_beyond_the_cap_hold_up_no_send() {
    let Some(shard) = ring_shard_of(64, 16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    // The cap is 128 - 16 = 112: more idle reads than that.
    let idle: Vec<_> = (0..130).map(|_| source_pair()).collect();
    let (_mine, src, mut peer) = source_pair();
    let arrived = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&arrived);
    let reader = std::thread::spawn(move || {
        let mut got = [0u8; 1];
        peer.read_exact(&mut got).unwrap();
        seen.store(true, Ordering::SeqCst);
        peer
    });
    shard.block_on(async {
        let mut bufs = vec![[0u8; 1]; idle.len()];
        poll_fn(|cx| {
            for ((_, source, _), buf) in idle.iter().zip(bufs.iter_mut()) {
                assert!(source.poll_read(cx, buf, false).is_pending());
            }
            Poll::Ready(())
        })
        .await;
        poll_fn(|cx| src.poll_write(cx, b"q")).await.unwrap();
        // The byte must reach the peer while the shard keeps running.
        let deadline = Instant::now() + DEADLINE;
        while !arrived.load(Ordering::SeqCst) && Instant::now() < deadline {
            crate::time::sleep(Duration::from_millis(1)).await;
        }
    });
    assert!(
        arrived.load(Ordering::SeqCst),
        "the send waited behind idle receives"
    );
    drop(reader.join().unwrap());
    for (_, source, _) in &idle {
        source.retire();
    }
    src.retire();
}

/// One-shot receives (with no receive group, each read is one into a buffer of the pool) that
/// wait on idle connections count against the cap no more than multishot ones: a send still goes.
#[test]
fn idle_one_shot_receives_beyond_the_cap_hold_up_no_send() {
    use crate::rt::backend::Backend;
    use crate::rt::shard::{Limits, Shard};
    use crate::syscore::linux::iouring::ring::RingConfig;
    let shard = match Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: Backend::Ring(RingConfig::new(64)),
    }) {
        Ok(shard) => shard,
        Err(err) => {
            eprintln!("skipping: io_uring unavailable: {err}");
            return;
        }
    };
    let _current = context::enter(&shard);
    // No group: the cap is the 128 completions of the ring, and more idle reads than that.
    let idle: Vec<_> = (0..130).map(|_| source_pair()).collect();
    let (_mine, src, mut peer) = source_pair();
    let arrived = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&arrived);
    let reader = std::thread::spawn(move || {
        let mut got = [0u8; 1];
        peer.read_exact(&mut got).unwrap();
        seen.store(true, Ordering::SeqCst);
        peer
    });
    shard.block_on(async {
        let mut bufs = vec![[0u8; 1]; idle.len()];
        poll_fn(|cx| {
            for ((_, source, _), buf) in idle.iter().zip(bufs.iter_mut()) {
                assert!(source.poll_read(cx, buf, false).is_pending());
            }
            Poll::Ready(())
        })
        .await;
        poll_fn(|cx| src.poll_write(cx, b"q")).await.unwrap();
        // The byte must reach the peer while the shard keeps running.
        let deadline = Instant::now() + DEADLINE;
        let mut pause = Duration::from_millis(1);
        while !arrived.load(Ordering::SeqCst) && Instant::now() < deadline {
            crate::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(20));
        }
    });
    assert!(
        arrived.load(Ordering::SeqCst),
        "the send waited behind idle one-shot receives"
    );
    drop(reader.join().unwrap());
    for (_, source, _) in &idle {
        source.retire();
    }
    src.retire();
}
