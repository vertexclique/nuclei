//! A full operation table ends the operation with `ENOMEM`, which no reader mistakes for a
//! receive group running dry or for a refused zero-copy send, and a completion the kernel
//! dropped (`EBADR` from an enter) is counted and said without stopping the shard; the shard then
//! recovers: the operation whose completion was dropped ends with an error, the others are made
//! again, and a completion that comes late is safe (`super::recover`).

use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::Poll;
use std::io::Write;
use std::time::{Duration, Instant};

use super::REFUSE_INSERT_FOR_TEST;
use super::keeper::{FILE_WAIT_FOR_TEST, SKIP_CANCEL_FOR_TEST};
use super::recover::{DROP_NEXT_FOR_TEST, SWEEP_WAIT_FOR_TEST};
use super::tests::{pipe_read_out, ring_shard};
use super::tests_recv::{pattern, read_exact, ring_shard_of, source_pair};
use crate::rt::context;
use crate::rt::park::ring::ENTER_EBADR_FOR_TEST;

/// How long a read may take; far above what an idle shard needs.
const DEADLINE: Duration = Duration::from_secs(5);

#[test]
fn a_full_table_ends_a_receive_with_enomem_and_not_as_a_dry_group() {
    let Some(shard) = ring_shard_of(64, 16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = source_pair();
    let data = pattern(10_000);
    REFUSE_INSERT_FOR_TEST.with(|n| n.set(1));
    shard.block_on(async {
        let mut one = [0u8; 16];
        let refused =
            crate::time::timeout(DEADLINE, poll_fn(|cx| src.poll_read(cx, &mut one, false)))
                .await
                .expect("the read hung on a full table");
        let err = refused.expect_err("the read of a full table answered bytes");
        assert_eq!(err.raw_os_error(), Some(libc::ENOMEM), "{err}");
    });
    let (_, _, dry) = shard.ring().unwrap().multishot_counts();
    assert_eq!(dry, 0, "the refused receive was counted as a dry group");
    assert_eq!(REFUSE_INSERT_FOR_TEST.with(|n| n.get()), 0);
    let sent = data.clone();
    let writer = std::thread::spawn(move || {
        peer.write_all(&sent).unwrap();
        peer
    });
    shard.block_on(async {
        let mut got = vec![0u8; data.len()];
        crate::time::timeout(DEADLINE, read_exact(&src, &mut got))
            .await
            .expect("the bytes never came");
        assert!(got == data, "the bytes arrived changed");
    });
    let (_, _, dry) = shard.ring().unwrap().multishot_counts();
    assert_eq!(dry, 0, "a receive was counted as a dry group");
    drop(writer.join().unwrap());
    src.retire();
}

#[test]
fn a_completion_the_kernel_dropped_is_counted_and_the_shard_goes_on() {
    let Some(shard) = ring_shard_of(64, 16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = source_pair();
    let data = pattern(10_000);
    let before = crate::stats().completions_dropped;
    ENTER_EBADR_FOR_TEST.with(|n| n.set(2));
    let sent = data.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        peer.write_all(&sent).unwrap();
        peer
    });
    shard.block_on(async {
        let mut got = vec![0u8; data.len()];
        crate::time::timeout(DEADLINE, read_exact(&src, &mut got))
            .await
            .expect("the bytes never came");
        assert!(got == data, "the bytes arrived changed");
    });
    assert_eq!(
        ENTER_EBADR_FOR_TEST.with(|n| n.get()),
        0,
        "the waits never met it"
    );
    assert!(
        crate::stats().completions_dropped >= before + 2,
        "the dropped completions were not counted"
    );
    drop(writer.join().unwrap());
    src.retire();
}

/// The error a swept operation ends with.
const DROPPED: &str = "the kernel dropped this operation's completion";

/// Waits for `done` to hold, with a deadline and a bounded backoff.
async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    let mut wait = Duration::from_millis(1);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        crate::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_millis(20));
    }
}

#[test]
fn a_dropped_completion_ends_its_operation_with_an_error() {
    let Some(shard) = ring_shard(8) else { return };
    let ring = shard.ring().unwrap();
    let _current = context::enter(&shard);
    let (_mine_a, a, mut peer_a) = source_pair();
    let (_mine_b, b, mut peer_b) = source_pair();
    SWEEP_WAIT_FOR_TEST.with(|wait| wait.set(Some(Duration::from_millis(40))));
    let before = crate::stats().completions_dropped;
    let data = pattern(300);
    shard.block_on(async {
        let (mut a_buf, mut b_buf) = ([0u8; 4096], [0u8; 4096]);
        let mut read_a = pin!(poll_fn(|cx| a.poll_read(cx, &mut a_buf, false)));
        let mut read_b = pin!(poll_fn(|cx| b.poll_read(cx, &mut b_buf, false)));
        let pending = poll_fn(|cx| {
            Poll::Ready((
                read_a.as_mut().poll(cx).is_pending(),
                read_b.as_mut().poll(cx).is_pending(),
            ))
        })
        .await;
        assert_eq!(
            pending,
            (true, true),
            "a read answered before any byte came"
        );
        // The next wait hands both reads to the kernel.
        crate::time::sleep(Duration::from_millis(20)).await;
        DROP_NEXT_FOR_TEST.with(|drop| drop.set(true));
        peer_a.write_all(&data[..64]).unwrap();
        let err = crate::time::timeout(DEADLINE, read_a.as_mut())
            .await
            .expect("the read whose completion was dropped never ended")
            .expect_err("the read answered bytes whose completion was dropped");
        assert_eq!(err.to_string(), DROPPED);
        assert!(
            !DROP_NEXT_FOR_TEST.with(|drop| drop.get()),
            "no completion was dropped"
        );
        // The other read was cancelled and made again: it still gets its own bytes.
        peer_b.write_all(&data).unwrap();
        let n = crate::time::timeout(DEADLINE, read_b.as_mut())
            .await
            .expect("the other read hung")
            .expect("the other read failed");
        assert!(n > 0 && n <= data.len());
        let mut got = b_buf[..n].to_vec();
        let mut rest = vec![0u8; data.len() - n];
        crate::time::timeout(DEADLINE, read_exact(&b, &mut rest))
            .await
            .expect("the rest of the bytes never came");
        got.extend_from_slice(&rest);
        assert!(got == data, "the bytes of the other read arrived changed");
        until("the recovery never ended", || !ring.recovering()).await;
    });
    SWEEP_WAIT_FOR_TEST.with(|wait| wait.set(None));
    assert!(
        crate::stats().completions_dropped > before,
        "the dropped completion was not counted"
    );
    assert_eq!(ring.swept_sources(), 1);
    assert_eq!(
        ring.in_flight(),
        ring.table_len_for_test(),
        "a swept operation is still in a table"
    );
    a.retire();
    b.retire();
}

#[test]
fn a_late_completion_after_the_sweep_is_safe() {
    let Some(shard) = ring_shard(8) else { return };
    let ring = shard.ring().unwrap();
    SKIP_CANCEL_FOR_TEST.with(|skip| skip.set(true));
    FILE_WAIT_FOR_TEST.with(|wait| wait.set(Some(Duration::from_millis(100))));
    SWEEP_WAIT_FOR_TEST.with(|wait| wait.set(Some(Duration::from_millis(20))));
    let mut pipe = pipe_read_out(&shard);
    let _current = context::enter(&shard);
    let (_mine, src, mut peer) = source_pair();
    let late = pattern(64);
    let next = pattern(200);
    shard.block_on(async {
        // A file's call the cancel does not end: swept once its wait ran out.
        ring.recover_for_test();
        until("the file call was never swept", || ring.swept_files() == 1).await;
        assert_eq!(
            ring.in_flight(),
            0,
            "a swept call still counts as in flight"
        );
        std::fs::File::from(pipe.write_end.try_clone().unwrap())
            .write_all(&late)
            .unwrap();
        until("the late completion never came", || ring.swept_files() == 0).await;
        let end = pipe.job.poll(std::task::Waker::noop());
        let Poll::Ready((Err(err), _)) = end else {
            panic!("the swept call's slot did not hold the error");
        };
        assert_eq!(err.to_string(), DROPPED);

        // A source's operation the same way.
        let mut buf = [0u8; 4096];
        let mut first = pin!(poll_fn(|cx| src.poll_read(cx, &mut buf, false)));
        let pending = poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx).is_pending())).await;
        assert!(pending, "a read answered before any byte came");
        crate::time::sleep(Duration::from_millis(20)).await;
        ring.recover_for_test();
        let err = crate::time::timeout(DEADLINE, first.as_mut())
            .await
            .expect("the swept read never ended")
            .expect_err("the swept read answered bytes");
        assert_eq!(err.to_string(), DROPPED);
        assert_eq!(ring.swept_sources(), 1);
        assert_eq!(
            ring.in_flight(),
            0,
            "a swept read still counts as in flight"
        );
        // The swept receive is still in the kernel: these bytes are its, and it ends with them.
        peer.write_all(&late).unwrap();
        until("the late completion never came", || {
            ring.swept_sources() == 0
        })
        .await;
        assert_eq!(ring.table_len_for_test(), 0);
        // A new read gets exactly the next bytes.
        peer.write_all(&next).unwrap();
        let mut got = vec![0u8; next.len()];
        crate::time::timeout(DEADLINE, read_exact(&src, &mut got))
            .await
            .expect("the next bytes never came");
        assert!(got == next, "the next bytes arrived changed");
    });
    SKIP_CANCEL_FOR_TEST.with(|skip| skip.set(false));
    FILE_WAIT_FOR_TEST.with(|wait| wait.set(None));
    SWEEP_WAIT_FOR_TEST.with(|wait| wait.set(None));
    src.retire();
}
