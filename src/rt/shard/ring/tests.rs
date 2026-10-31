//! A shard's ring with real sources: what goes past a full table, and that
//! nothing is left in the ring once its sources are gone.

use core::future::poll_fn;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use super::TABLE_FOR_TEST;
use crate::io::source::{Kind, Source};
use crate::io::sys;
use crate::rt::backend::Backend;
use crate::rt::context;
use crate::rt::shard::{Limits, Shard};
use crate::syscore::linux::iouring::ring::RingConfig;

/// A shard on a ring whose table holds `files` fixed files, or `None` where
/// the kernel gives no ring.
fn ring_shard(files: u32) -> Option<Shard> {
    TABLE_FOR_TEST.with(|t| t.set(Some(files)));
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

/// A socket of a pair with its source, and the pair's other end.
fn source_pair() -> (UnixStream, Arc<Source>, UnixStream) {
    let (mine, peer) = UnixStream::pair().unwrap();
    sys::set_nonblocking(mine.as_raw_fd()).unwrap();
    let src = Source::new(mine.as_raw_fd(), Kind::Stream);
    (mine, src, peer)
}

/// Three sources on a ring with one fixed file: the first takes it, the
/// others run on their own descriptors, which the ring never copies, and
/// bytes go both ways on all three. Once they retire, the ring holds no file
/// of theirs.
#[test]
fn sources_past_a_full_table_run_on_their_own_descriptors() {
    let Some(shard) = ring_shard(1) else { return };
    let _current = context::enter(&shard);
    let pairs: Vec<_> = (0..3).map(|_| source_pair()).collect();
    shard.block_on(async {
        for (i, (_, src, peer)) in pairs.iter().enumerate() {
            let hello = [b'a' + i as u8; 3];
            let mut rest = &hello[..];
            while !rest.is_empty() {
                let n = poll_fn(|cx| src.poll_write(cx, rest)).await.unwrap();
                rest = &rest[n..];
            }
            poll_fn(|cx| src.poll_flush(cx)).await.unwrap();
            let mut got = [0u8; 3];
            let mut peer = peer;
            peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            peer.read_exact(&mut got).unwrap();
            assert_eq!(got, hello, "the bytes of source {i} went astray");
            peer.write_all(b"back").unwrap();
            let mut buf = [0u8; 4];
            let n = poll_fn(|cx| src.poll_read(cx, &mut buf, false))
                .await
                .unwrap();
            assert_eq!(&buf[..n], &b"back"[..n], "source {i} read another's bytes");
        }
    });
    let ring = shard.ring().expect("a ring shard");
    assert_eq!(
        ring.files_in_use(),
        (3, 2),
        "one fixed file, two slots naming the sources' own descriptors"
    );

    for (_, src, _) in &pairs {
        src.retire();
    }
    shard.block_on(async {});
    assert_eq!(
        ring.files_in_use(),
        (0, 0),
        "the ring kept a retired source's file"
    );
}

/// A source with a read out on the ring retires: the read is cancelled
/// before the retire returns, so bytes that come after it stay in the
/// socket for whoever reads it next.
#[test]
fn a_retire_returns_only_once_the_rings_read_is_cancelled() {
    let Some(shard) = ring_shard(4) else { return };
    let _current = context::enter(&shard);
    let (mine, src, peer) = source_pair();
    shard.block_on(async {
        let mut buf = [0u8; 8];
        let mut read = core::pin::pin!(poll_fn(|cx| src.poll_read(cx, &mut buf, false)));
        let polled = poll_fn(|cx| core::task::Poll::Ready(read.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "the peer wrote nothing yet");
        // A write that completes through the ring takes the queued read to
        // the kernel with it.
        poll_fn(|cx| src.poll_write(cx, b"!")).await.unwrap();
        poll_fn(|cx| src.poll_flush(cx)).await.unwrap();
    });
    let mut bang = [0u8; 1];
    (&peer).read_exact(&mut bang).unwrap();
    src.retire();
    (&peer).write_all(b"after").unwrap();
    let mut got = [0u8; 8];
    mine.set_nonblocking(false).unwrap();
    mine.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let n = (&mine).read(&mut got).unwrap();
    assert_eq!(
        &got[..n],
        b"after",
        "the retired source's read took the bytes"
    );
    assert_eq!(shard.ring().unwrap().files_in_use(), (0, 0));
}

/// A shard that goes with a read out on its ring cancels it and waits for
/// its completion: no operation is left with memory the kernel may write.
#[test]
fn a_shard_that_goes_waits_for_its_cancelled_operations() {
    let Some(shard) = ring_shard(4) else { return };
    let (_mine, src, _peer) = source_pair();
    let before = super::leaked();
    {
        let _current = context::enter(&shard);
        shard.block_on(async {
            let mut buf = [0u8; 8];
            let mut read = core::pin::pin!(poll_fn(|cx| src.poll_read(cx, &mut buf, false)));
            let polled = poll_fn(|cx| core::task::Poll::Ready(read.as_mut().poll(cx))).await;
            assert!(polled.is_pending(), "the peer wrote nothing yet");
            // A write that completes through the ring takes the queued read
            // to the kernel with it.
            poll_fn(|cx| src.poll_write(cx, b"!")).await.unwrap();
            poll_fn(|cx| src.poll_flush(cx)).await.unwrap();
        });
    }
    drop(shard);
    assert_eq!(
        super::leaked(),
        before,
        "an operation was left to the kernel"
    );
    // Evicted, the source registers again where it is polled next.
    src.retire();
}

/// Under a memory limit no ring fits in, a runtime that `auto` gave rings
/// runs its worker on the poller, and its IO works; one that asks for
/// io_uring by name fails with the reason. Alone in its process: the limit
/// cannot be raised again.
#[test]
fn a_shard_short_of_memory_for_a_ring_runs_the_poller_under_auto() {
    if !crate::alone::alone(
        "rt::shard::ring::tests::a_shard_short_of_memory_for_a_ring_runs_the_poller_under_auto",
    ) {
        return;
    }
    if std::env::var("NUCLEI_BACKEND").is_ok_and(|b| !b.is_empty()) {
        return;
    }
    // `auto` decided while memory was there.
    if !crate::rt::backend::chosen().is_ok_and(|b| b.rings()) {
        eprintln!("skipping: io_uring is not usable here");
        return;
    }
    let none = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain data in, nothing kept.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &none) }, 0);
    if super::ShardRing::new(&RingConfig::new(64), 64).is_ok() {
        eprintln!("skipping: this kernel makes rings past a zero memory-lock limit");
        return;
    }
    let rt = crate::runtime::Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .expect("a runtime that may fall back to the poller starts");
    let (mine, src, peer) = source_pair();
    let (backend, got) = rt.block_on(async move {
        crate::runtime::spawn(async move {
            let mut rest = &b"hi"[..];
            while !rest.is_empty() {
                let n = poll_fn(|cx| src.poll_write(cx, rest)).await.unwrap();
                rest = &rest[n..];
            }
            poll_fn(|cx| src.poll_flush(cx)).await.unwrap();
            let mut got = [0u8; 2];
            let mut peer = &peer;
            peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            peer.read_exact(&mut got).unwrap();
            src.retire();
            (crate::Proactor::backend(), got)
        })
        .await
    });
    drop(mine);
    assert_eq!(backend, crate::IoBackend::Epoll);
    assert_eq!(&got, b"hi");
    let Err(err) = crate::runtime::Builder::new()
        .worker_threads(1)
        .backend(crate::IoBackend::IoUring)
        .build()
    else {
        panic!("a runtime that asked for io_uring by name started without memory for a ring");
    };
    assert_eq!(err.raw_os_error(), Some(libc::ENOMEM), "{err}");
}
