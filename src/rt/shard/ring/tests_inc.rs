//! A receive group whose buffers the kernel fills in pieces: a long stream
//! comes whole and in order, small and large reads mix, and every chunk's
//! buffer comes back.

use std::io::Write;

use super::tests_recv::{ring_shard_of, source_pair};
use crate::rt::context;
use crate::rt::shard::Shard;
use crate::rt::shard::recv::INCREMENTAL_FOR_TEST;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// A shard whose group asks for pieces (`incremental`) or never does, or
/// `None` where the kernel gives no such group.
fn shard_with(incremental: bool) -> Option<Shard> {
    INCREMENTAL_FOR_TEST.with(|t| t.set((!incremental).then_some(false)));
    let shard = ring_shard_of(64, 32, 16 * 1024);
    INCREMENTAL_FOR_TEST.with(|t| t.set(None));
    let shard = shard?;
    let (_, got) = shard.ring()?.live_chunks();
    if got != incremental {
        eprintln!("skipping: the kernel gives no incremental buffers");
        return None;
    }
    Some(shard)
}

async fn read_some(src: &std::sync::Arc<crate::io::source::Source>, out: &mut [u8]) -> usize {
    core::future::poll_fn(|cx| src.poll_read(cx, out, false))
        .await
        .unwrap()
}

/// Reads exactly `want` bytes in reads of `sizes` in turn, checking them.
async fn drain(src: &std::sync::Arc<crate::io::source::Source>, want: &[u8], sizes: &[usize]) {
    let mut at = 0;
    let mut turn = 0;
    while at < want.len() {
        let size = sizes[turn % sizes.len()].min(want.len() - at);
        turn += 1;
        let mut buf = vec![0u8; size];
        let n = read_some(src, &mut buf).await;
        assert!(n > 0, "the stream ended after {at} bytes");
        assert!(buf[..n] == want[at..at + n], "bytes changed at {at}");
        at += n;
    }
}

fn run(incremental: bool, sizes: &[usize]) {
    let Some(shard) = shard_with(incremental) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = source_pair();
    let data = pattern(6 * 1024 * 1024 + 5);
    let sent = data.clone();
    let writer = std::thread::spawn(move || {
        peer.write_all(&sent).unwrap();
        peer
    });
    shard.block_on(drain(&src, &data, sizes));
    let peer = writer.join().unwrap();
    let ring = shard.ring().expect("a ring shard");
    // Every chunk was dropped, so every buffer is back or still with the
    // kernel for its next piece.
    assert_eq!(ring.live_chunks().0, 0, "a chunk was never given back");
    assert_eq!(ring.held(), 0, "bytes of the group are still out");
    drop(peer);
    src.retire();
    shard.block_on(async {});
    assert_eq!(ring.live_chunks().0, 0);
    assert_eq!(ring.held(), 0);
}

#[test]
fn a_long_stream_comes_whole_and_in_order_through_pieces() {
    run(true, &[16 * 1024]);
}

#[test]
fn small_and_large_reads_mix_over_pieces() {
    run(true, &[1, 7, 64 * 1024, 3, 1500, 256 * 1024, 1]);
}

#[test]
fn a_long_stream_comes_whole_and_in_order_through_whole_buffers() {
    run(false, &[16 * 1024]);
}

#[test]
fn small_and_large_reads_mix_over_whole_buffers() {
    run(false, &[1, 7, 64 * 1024, 3, 1500, 256 * 1024, 1]);
}
