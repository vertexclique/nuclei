//! A large stream write (16 KiB or more, `BEHIND_MIN`) that cannot go at
//! once waits for the socket instead of being copied into the write-behind
//! (`write_direct`, proofs/tla/ring/RingSend.tla), on a ring and on a poller
//! alike, and keeps its place behind bytes written before it; a write that
//! never waits (`try_write`) still copies it.

use super::*;

/// What the peer's source reads of `total` bytes, on a task of the runtime.
fn reader(src: &Arc<Source>, total: usize) -> impl Future<Output = Vec<u8>> {
    let src = Arc::clone(src);
    crate::runtime::spawn(async move {
        let mut got = vec![0u8; total];
        read_exact(&src, &mut got).await.unwrap();
        got
    })
}

#[test]
fn a_large_write_on_a_full_socket_waits_and_copies_nothing() {
    let rt = runtime(None);
    let (a, b) = pair();
    let mut expected = fill(a.io());
    let chunk = pattern(64 * 1024);
    expected.extend_from_slice(&chunk);
    let total = expected.len();
    let got = rt.block_on(async {
        let mut write = Box::pin(write_all(&a.src, &chunk));
        pends(&mut write).await;
        assert!(!a.src.has_unsent(), "the large write was copied behind");
        let read = reader(&b.src, total);
        within(write).await.unwrap();
        within(read).await
    });
    assert!(
        got == expected,
        "the peer got other bytes than were written"
    );
}

#[test]
fn a_large_write_waits_for_the_bytes_behind_and_keeps_their_order() {
    let rt = runtime(None);
    let (a, b) = pair();
    let mut expected = fill(a.io());
    let (small, large) = (pattern(1000), pattern(100_000));
    expected.extend_from_slice(&small);
    expected.extend_from_slice(&large);
    let total = expected.len();
    let got = rt.block_on(async {
        within(write_all(&a.src, &small)).await.unwrap();
        assert!(a.src.has_unsent(), "the small write was not copied behind");
        let mut write = Box::pin(write_all(&a.src, &large));
        pends(&mut write).await;
        let read = reader(&b.src, total);
        within(write).await.unwrap();
        within(flush(&a.src)).await.unwrap();
        within(read).await
    });
    assert!(got == expected, "the writes went out of order");
}

#[test]
fn a_write_that_never_waits_copies_a_large_chunk_behind() {
    let rt = runtime(None);
    let (a, b) = pair();
    let mut expected = fill(a.io());
    let chunk = pattern(64 * 1024);
    let total = rt.block_on(async {
        let mut cx = Context::from_waker(Waker::noop());
        let taken = match a
            .src
            .poll_write_behind(&mut cx, &[io::IoSlice::new(&chunk)])
        {
            Poll::Ready(taken) => taken.unwrap(),
            Poll::Pending => panic!("a write that never waits waited"),
        };
        assert!(taken > 0, "the write-behind took nothing");
        assert!(a.src.has_unsent(), "the chunk was not copied behind");
        expected.extend_from_slice(&chunk[..taken]);
        expected.len()
    });
    let got = rt.block_on(async {
        let read = reader(&b.src, total);
        within(flush(&a.src)).await.unwrap();
        within(read).await
    });
    assert!(
        got == expected,
        "the peer got other bytes than were written"
    );
}

#[test]
fn a_dropped_large_write_leaves_nothing_to_send() {
    let rt = runtime(None);
    let (a, _b) = pair();
    fill(a.io());
    rt.block_on(async {
        let chunk = pattern(64 * 1024);
        let mut write = Box::pin(write_all(&a.src, &chunk));
        pends(&mut write).await;
        drop(write);
        assert!(!a.src.has_unsent(), "a dropped large write left bytes");
        // Nothing waits to go: the flush ends though the peer reads nothing.
        within(flush(&a.src)).await.unwrap();
    });
}
