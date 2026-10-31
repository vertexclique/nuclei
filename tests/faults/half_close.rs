//! A peer that shuts its writing half: every read ends with 0 after every
//! byte it sent, and keeps answering 0, while our writes still reach it.
//! Our own shutdown: the peer reads every byte before its end, a write after
//! it fails as std's does, and our reads go on.

use std::io::{self, Read, Write};
use std::net::Shutdown;

use crate::reset::{READERS, Reader, WRITERS, Writer, read_once, read_to_end_as, write_as};
use crate::support::*;

pub async fn peer_half_close<S: Stream>() {
    for sent in [0, 1, 4096, 100_000] {
        let bytes = pattern(sent as u64 + 11, sent);
        for how in READERS {
            for writer in WRITERS {
                let (ours, mut peer) = S::pair(false);
                peer.write_all(&bytes).unwrap();
                S::shutdown_std(&peer, Shutdown::Write).unwrap();
                let what = format!(
                    "{}: {sent} bytes and a half close, {how:?} reads, a {writer:?} write",
                    S::NAME
                );
                let (got, end) = within(&what, read_to_end_as(&ours, how, 4096)).await;
                same_bytes(&what, &got, &bytes);
                assert_eq!(end, Answer::Ok(0), "{what}: the read at the end");
                for _ in 0..3 {
                    let again = within(&what, read_once(&ours, how, 4096)).await;
                    assert_eq!(
                        Answer::of(&again, Vec::len),
                        Answer::Ok(0),
                        "{what}: past the end"
                    );
                }
                let back = pattern(sent as u64 + 12, 70_000);
                let reader = std::thread::spawn(move || {
                    let mut peer = peer;
                    let mut buf = vec![0u8; 70_000];
                    peer.read_exact(&mut buf).map(|()| buf)
                });
                within(&what, write_as(&ours, writer, &back)).await.unwrap();
                let echoed = joined(reader).await.unwrap();
                same_bytes(&what, &echoed, &back);
            }
        }
    }
}

/// `read_exact` past a half close: the bytes that came, then the end.
pub async fn read_exact_past_a_half_close<S: Stream>() {
    let bytes = pattern(5, 3000);
    let (mut std_ours, mut std_peer) = S::std_pair(false);
    std_peer.write_all(&bytes).unwrap();
    S::shutdown_std(&std_peer, Shutdown::Write).unwrap();
    let mut buf = vec![0u8; 3001];
    let want = std_ours.read_exact(&mut buf).unwrap_err().kind();
    let (ours, mut peer) = S::pair(false);
    peer.write_all(&bytes).unwrap();
    S::shutdown_std(&peer, Shutdown::Write).unwrap();
    let got = within("read_exact past a half close", read_exact(&ours, 3001)).await;
    assert_eq!(
        got.unwrap_err().kind(),
        want,
        "read_exact past a half close"
    );
}

/// What std answers to a write after its own shutdown, and to a second
/// shutdown.
fn oracle_own_shutdown<S: Stream>() -> (Answer, Answer) {
    let (mut ours, _peer) = S::std_pair(false);
    ours.write_all(b"bytes").unwrap();
    S::shutdown_std(&ours, Shutdown::Write).unwrap();
    let twice = S::shutdown_std(&ours, Shutdown::Write);
    let wrote = ours.write_all(b"more");
    (Answer::of(&wrote, |_| 0), Answer::of(&twice, |_| 0))
}

pub async fn own_shutdown<S: Stream>() {
    let (want_write, want_twice) = oracle_own_shutdown::<S>();
    for writer in WRITERS {
        let bytes = pattern(13, 200_000);
        let (ours, peer) = S::pair(false);
        let what = format!("{}: a {writer:?} write, then our shutdown", S::NAME);
        let reader = std::thread::spawn(move || {
            let mut peer = peer;
            let got = std_read_until_end(&mut peer, 8192);
            (peer, got)
        });
        within(&what, write_as(&ours, writer, &bytes))
            .await
            .unwrap();
        let shut = within(&what, close(&ours)).await;
        assert!(shut.is_ok(), "{what}: the shutdown: {shut:?}");
        let (mut peer, (got, end)) = joined(reader).await;
        same_bytes(&what, &got, &bytes);
        assert!(matches!(end, Ok(0)), "{what}: the peer's end: {end:?}");
        let twice = within(&what, close(&ours)).await;
        assert_eq!(
            Answer::of(&twice, |_| 0),
            want_twice,
            "{what}: a second shutdown"
        );
        let wrote = within(&what, write_as(&ours, Writer::Plain, b"more")).await;
        assert_eq!(
            Answer::of(&wrote, |_| 0),
            want_write,
            "{what}: a write after it"
        );
        let back = pattern(14, 50_000);
        peer.write_all(&back).unwrap();
        S::shutdown_std(&peer, Shutdown::Write).unwrap();
        let (got, end) = within(&what, read_to_end_as(&ours, Reader::Owned, 4096)).await;
        same_bytes(&format!("{what}: our reads after it"), &got, &back);
        assert_eq!(end, Answer::Ok(0), "{what}: our read at the end");
    }
}

/// Both halves shut, ours first: the reads and writes of each side answer
/// as std's do once the stream is closed both ways.
pub async fn both_halves_shut<S: Stream>() -> io::Result<()> {
    let (ours, peer) = S::pair(false);
    within("our shutdown", close(&ours)).await?;
    S::shutdown_std(&peer, Shutdown::Write)?;
    let (got, end) = within(
        "reads after both shutdowns",
        read_to_end_as(&ours, Reader::Borrowed, 64),
    )
    .await;
    assert!(got.is_empty());
    assert_eq!(end, Answer::Ok(0));
    Ok(())
}

macro_rules! half_close_cases {
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
            fn a_peer_half_close_ends_every_read_after_every_byte() {
                run(
                    concat!(
                        "half_close::",
                        stringify!($module),
                        "::a_peer_half_close_ends_every_read_after_every_byte"
                    ),
                    super::peer_half_close::<$ty>,
                );
            }

            #[test]
            fn read_exact_past_a_half_close_ends_as_std() {
                run(
                    concat!(
                        "half_close::",
                        stringify!($module),
                        "::read_exact_past_a_half_close_ends_as_std"
                    ),
                    super::read_exact_past_a_half_close::<$ty>,
                );
            }

            #[test]
            fn our_shutdown_delivers_every_byte_and_reads_go_on() {
                run(
                    concat!(
                        "half_close::",
                        stringify!($module),
                        "::our_shutdown_delivers_every_byte_and_reads_go_on"
                    ),
                    super::own_shutdown::<$ty>,
                );
            }

            #[test]
            fn both_halves_shut_end_the_reads() {
                run(
                    concat!(
                        "half_close::",
                        stringify!($module),
                        "::both_halves_shut_end_the_reads"
                    ),
                    async || super::both_halves_shut::<$ty>().await.unwrap(),
                );
            }

            #[test]
            fn half_closes_hold_nothing() {
                let name = concat!(
                    "half_close::",
                    stringify!($module),
                    "::half_closes_hold_nothing"
                );
                if alone(name) {
                    holds_steady(name, 3, || async {
                        super::own_shutdown::<$ty>().await;
                        super::both_halves_shut::<$ty>().await.unwrap();
                    });
                }
            }
        }
    };
}

half_close_cases!(tcp, nuclei::net::TcpStream);
#[cfg(unix)]
half_close_cases!(unix, nuclei::net::UnixStream);
