//! Short reads and writes: both ends' socket buffers as small as the system
//! lets them be, so every call moves a little, and transfers of every size
//! from one byte to one MiB around each threshold of the runtime (the pool's
//! size classes, the direct-call and owned-datagram thresholds, the
//! write-behind's least, the zero-copy size), through every writer and
//! reader. The bytes arrive exactly, in order, once.

use crate::reset::{READERS, Reader, WRITERS, Writer, read_to_end_as, write_as};
use crate::support::*;

/// One byte, then each threshold less one, at it and past it.
pub const SIZES: [usize; 22] = [
    1,
    2,
    511,
    512,
    513,
    4095,
    4096,
    4097,
    12_287,
    12_288,
    12_289,
    16_383,
    16_384,
    16_385,
    65_535,
    65_536,
    65_537,
    262_144,
    (1 << 20) - 4096,
    (1 << 20) - 1,
    1 << 20,
    (1 << 20) + 1,
];

/// The read sizes a sweep takes in turn.
const CHUNKS: [usize; 5] = [1, 100, 4096, 16_384, 65_536];

/// A transfer between two ends of nuclei's, each writer with each reader in
/// turn, the reader reading while the writer writes.
async fn sweep<S: Stream>(tiny: bool) {
    for (i, size) in SIZES.into_iter().enumerate() {
        for (j, writer) in WRITERS.into_iter().enumerate() {
            // Past 64 KiB each size takes one writer in turn: small buffers move
            // a MiB in seconds.
            if size > 65_537 && j != i % WRITERS.len() {
                continue;
            }
            let reader = READERS[(i + j) % READERS.len()];
            let chunk = CHUNKS[(i + j) % CHUNKS.len()];
            transfer::<S>(tiny, size, writer, reader, chunk).await;
        }
    }
}

async fn transfer<S: Stream>(
    tiny: bool,
    size: usize,
    writer: Writer,
    reader: Reader,
    chunk: usize,
) {
    let what = format!(
        "{}: {size} bytes, tiny {tiny}, a {writer:?} write, {reader:?} reads of {chunk}",
        S::NAME
    );
    let bytes = pattern(size as u64 * 7 + 3, size);
    let (ours, peer) = S::pair(tiny);
    let theirs = S::adopt(peer);
    let (wrote, (got, end)) = within(&what, async {
        nuclei::join!(
            async {
                write_as(&ours, writer, &bytes).await?;
                close(&ours).await
            },
            read_to_end_as(&theirs, reader, chunk)
        )
    })
    .await;
    assert!(wrote.is_ok(), "{what}: the write: {wrote:?}");
    assert_eq!(end, Answer::Ok(0), "{what}: the end");
    same_bytes(&what, &got, &bytes);
}

/// The same with the peer std's, reading or writing in pieces of its own.
async fn against_std<S: Stream>() {
    use std::io::Write;
    for size in SIZES
        .into_iter()
        .filter(|&s| s <= 262_144 || S::NAME != "tcp")
    {
        let bytes = pattern(size as u64 + 5, size);
        let what = format!("{}: {size} bytes to and from std, tiny", S::NAME);
        let (ours, peer) = S::pair(true);
        let reader = std::thread::spawn(move || {
            let mut peer = peer;
            let got = std_read_until_end(&mut peer, 777);
            (peer, got)
        });
        within(&what, write_as(&ours, Writer::Owned, &bytes))
            .await
            .unwrap();
        within(&what, close(&ours)).await.unwrap();
        let (mut peer, (got, end)) = joined(reader).await;
        assert!(end.is_ok(), "{what}: {end:?}");
        same_bytes(&what, &got, &bytes);
        let sent = bytes.clone();
        let writer = std::thread::spawn(move || {
            for piece in sent.chunks(1013) {
                peer.write_all(piece).unwrap();
            }
            S::shutdown_std(&peer, std::net::Shutdown::Write).unwrap();
            peer
        });
        let (got, end) = within(&what, read_to_end_as(&ours, Reader::Owned, 3000)).await;
        drop(joined(writer).await);
        assert_eq!(end, Answer::Ok(0), "{what}: the end");
        same_bytes(&what, &got, &bytes);
    }
}

macro_rules! short_cases {
    ($module:ident, $ty:ty) => {
        mod $module {
            use crate::support::*;

            fn run(name: &str, case: impl AsyncFnOnce()) {
                if alone(name) {
                    let rt = runtime();
                    rt.block_on(case());
                    rt.block_on(round_trip());
                }
            }

            #[test]
            fn every_size_through_the_smallest_buffers_arrives_exactly() {
                run(
                    concat!(
                        "short_io::",
                        stringify!($module),
                        "::every_size_through_the_smallest_buffers_arrives_exactly"
                    ),
                    async || super::sweep::<$ty>(true).await,
                );
            }

            #[test]
            fn every_size_through_the_default_buffers_arrives_exactly() {
                run(
                    concat!(
                        "short_io::",
                        stringify!($module),
                        "::every_size_through_the_default_buffers_arrives_exactly"
                    ),
                    async || super::sweep::<$ty>(false).await,
                );
            }

            #[test]
            fn every_size_to_and_from_std_arrives_exactly() {
                run(
                    concat!(
                        "short_io::",
                        stringify!($module),
                        "::every_size_to_and_from_std_arrives_exactly"
                    ),
                    super::against_std::<$ty>,
                );
            }

            #[test]
            fn short_transfers_hold_nothing() {
                let name = concat!(
                    "short_io::",
                    stringify!($module),
                    "::short_transfers_hold_nothing"
                );
                if alone(name) {
                    holds_steady(name, 2, || async {
                        for size in [1, 4097, 65_537, 300_000] {
                            for writer in crate::reset::WRITERS {
                                super::transfer::<$ty>(
                                    true,
                                    size,
                                    writer,
                                    crate::reset::Reader::Owned,
                                    4096,
                                )
                                .await;
                            }
                        }
                    });
                }
            }
        }
    };
}

short_cases!(tcp, nuclei::net::TcpStream);
#[cfg(unix)]
short_cases!(unix, nuclei::net::UnixStream);
