//! A peer that resets (TCP closed with `SO_LINGER` 0; a Unix stream closed
//! with bytes it never read) before, during and after our reads and writes,
//! a large write among them that is partly queued when the reset comes. Each
//! read answers what std's answers on the same kind of stream in the same
//! order of events; a write answers an error std's write could.

use core::future::poll_fn;
use core::pin::pin;
use core::task::Poll;
use std::io::{self, Read, Write};
use std::sync::mpsc;

use crate::support::*;

/// A byte the peer never reads, so that its close resets a Unix stream as
/// `SO_LINGER` 0 resets TCP.
const UNREAD: &[u8] = b"x";

#[derive(Clone, Copy, Debug)]
pub enum Reader {
    Borrowed,
    Owned,
    Vectored,
}

pub const READERS: [Reader; 3] = [Reader::Borrowed, Reader::Owned, Reader::Vectored];

#[derive(Clone, Copy, Debug)]
pub enum Writer {
    Plain,
    Vectored,
    Owned,
    SendFile,
}

pub const WRITERS: [Writer; 4] = [
    Writer::Plain,
    Writer::Vectored,
    Writer::Owned,
    Writer::SendFile,
];

/// One read of `chunk` bytes as `how` reads.
pub async fn read_once<S: Stream>(s: &S, how: Reader, chunk: usize) -> io::Result<Vec<u8>> {
    match how {
        Reader::Borrowed => {
            let mut buf = vec![0u8; chunk];
            let n = read_some(s, &mut buf).await?;
            buf.truncate(n);
            Ok(buf)
        }
        Reader::Owned => {
            let (result, buf) = s.read_owned(Vec::with_capacity(chunk)).await;
            let n = result?;
            assert_eq!(buf.len(), n, "an owned read answered another count");
            Ok(buf)
        }
        Reader::Vectored => {
            let half = chunk / 2;
            let (mut a, mut b) = (vec![0u8; half.max(1)], vec![0u8; chunk - half]);
            let n = {
                let mut bufs = [io::IoSliceMut::new(&mut a), io::IoSliceMut::new(&mut b)];
                read_vectored(s, &mut bufs).await?
            };
            a.extend_from_slice(&b);
            a.truncate(n);
            Ok(a)
        }
    }
}

/// Reads as `how` until a read answers 0 or an error: the bytes, and that
/// read's answer.
pub async fn read_to_end_as<S: Stream>(s: &S, how: Reader, chunk: usize) -> (Vec<u8>, Answer) {
    let mut got = Vec::new();
    loop {
        match read_once(s, how, chunk).await {
            Ok(bytes) if bytes.is_empty() => return (got, Answer::Ok(0)),
            Ok(bytes) => got.extend_from_slice(&bytes),
            Err(err) => return (got, Answer::of(&Err::<(), _>(err), |_| 0)),
        }
    }
}

/// Writes all of `bytes` as `how` writes, flushed: `Ok` once the kernel
/// took them.
pub async fn write_as<S: Stream>(s: &S, how: Writer, bytes: &[u8]) -> io::Result<()> {
    match how {
        Writer::Plain => write_all(s, bytes).await,
        Writer::Vectored => write_all_vectored(s, bytes, 3).await,
        Writer::Owned => {
            let (result, back) = s.write_owned(bytes.to_vec()).await;
            assert_eq!(
                back.len(),
                bytes.len(),
                "the owned write gave back another buffer"
            );
            result.map(drop)
        }
        Writer::SendFile => {
            let file = TempFile::with("send-file", bytes);
            let handle = nuclei::fs::File::open(&file.0).await?;
            let sent = s.send_file(&handle, 0, bytes.len() as u64).await?;
            assert_eq!(
                sent,
                bytes.len() as u64,
                "send_file stopped short of the file's end"
            );
            Ok(())
        }
    }
}

/// What std answers when the peer sends `sent` and resets before the first
/// read: the bytes, the read that ended them, and the next read.
fn oracle_reset_then_read<S: Stream>(sent: &[u8], chunk: usize) -> (Vec<u8>, Answer, Answer) {
    let (mut ours, mut peer) = S::std_pair(false);
    peer.write_all(sent).unwrap();
    ours.write_all(UNREAD).unwrap();
    S::reset(peer);
    let (got, end) = std_read_until_end(&mut ours, chunk);
    let mut buf = vec![0u8; chunk];
    let next = ours.read(&mut buf);
    (got, Answer::of(&end, |n| *n), Answer::of(&next, |n| *n))
}

pub async fn reset_before_a_read<S: Stream>() {
    for sent in [0, 1000, 30_000] {
        let bytes = pattern(sent as u64, sent);
        for how in READERS {
            for chunk in [1, 4096, 65_536] {
                let want = oracle_reset_then_read::<S>(&bytes, chunk);
                let (ours, mut peer) = S::pair(false);
                peer.write_all(&bytes).unwrap();
                write_all(&ours, UNREAD).await.unwrap();
                S::reset(peer);
                let what = format!(
                    "{}: {sent} bytes, a reset, then {how:?} reads of {chunk}",
                    S::NAME
                );
                let (got, end) = within(&what, read_to_end_as(&ours, how, chunk)).await;
                let next = within(&what, read_once(&ours, how, chunk)).await;
                same_bytes(&what, &got, &want.0);
                assert_eq!(end, want.1, "{what}: the read that ended the bytes");
                assert_eq!(
                    Answer::of(&next, Vec::len),
                    want.2,
                    "{what}: the read after it"
                );
            }
        }
    }
}

pub async fn reset_during_a_read<S: Stream>() {
    for sent in [0, 1000, 30_000] {
        let bytes = pattern(sent as u64 + 1, sent);
        for how in READERS {
            let want = oracle_reset_then_read::<S>(&bytes, 4096);
            let (ours, peer) = S::pair(false);
            write_all(&ours, UNREAD).await.unwrap();
            let (go, wait) = mpsc::channel::<()>();
            let to_send = bytes.clone();
            let peer = std::thread::spawn(move || {
                let mut peer = peer;
                wait.recv().unwrap();
                peer.write_all(&to_send).unwrap();
                S::reset(peer);
            });
            let what = format!(
                "{}: a {how:?} read out, then {sent} bytes and a reset",
                S::NAME
            );
            let mut reading = pin!(read_to_end_as(&ours, how, 4096));
            let early = poll_fn(|cx| Poll::Ready(reading.as_mut().poll(cx))).await;
            assert!(
                early.is_pending(),
                "{what}: the read ended before anything came"
            );
            go.send(()).unwrap();
            let (got, end) = within(&what, reading).await;
            joined(peer).await;
            let next = within(&what, read_once(&ours, how, 4096)).await;
            if cfg!(windows) {
                // Windows drops what a reset finds unread, so std's read after the reset gets
                // none of it, while a read already out takes what came before the reset: any
                // prefix, in order.
                same_bytes(&what, &got, &bytes[..got.len().min(bytes.len())]);
            } else {
                same_bytes(&what, &got, &want.0);
            }
            assert_eq!(end, want.1, "{what}: the read that ended the bytes");
            assert_eq!(
                Answer::of(&next, Vec::len),
                want.2,
                "{what}: the read after it"
            );
        }
    }
}

pub async fn reset_after_a_read<S: Stream>() {
    let bytes = pattern(7, 5000);
    for how in READERS {
        let (mut std_ours, mut std_peer) = S::std_pair(false);
        std_peer.write_all(&bytes).unwrap();
        let mut buf = vec![0u8; bytes.len()];
        std_ours.read_exact(&mut buf).unwrap();
        std_ours.write_all(UNREAD).unwrap();
        S::reset(std_peer);
        let want = std_ours.read(&mut buf);

        let (ours, mut peer) = S::pair(false);
        peer.write_all(&bytes).unwrap();
        let what = format!("{}: a {how:?} read of every byte, then a reset", S::NAME);
        let mut got = Vec::new();
        while got.len() < bytes.len() {
            let more = within(&what, read_once(&ours, how, bytes.len() - got.len())).await;
            got.extend_from_slice(&more.unwrap());
        }
        same_bytes(&what, &got, &bytes);
        write_all(&ours, UNREAD).await.unwrap();
        S::reset(peer);
        let after = within(&what, read_once(&ours, how, 4096)).await;
        assert_eq!(
            Answer::of(&after, Vec::len),
            Answer::of(&want, |n| *n),
            "{what}"
        );
    }
}

/// The reset is seen first by a read, as std's: a write then answers what
/// std's write answers on a stream whose reset was read, and a file sent what
/// the system's own call for it answers there where that differs (macOS).
pub async fn reset_then_a_write<S: Stream>() {
    let (mut std_ours, std_peer) = S::std_pair(false);
    std_ours.write_all(UNREAD).unwrap();
    S::reset(std_peer);
    let _ = std_read_until_end(&mut std_ours, 4096);
    let want_file = S::std_send_file(&std_ours);
    let want = std_ours.write_all(&pattern(1, 100_000));
    let want = Answer::of(&want, |_| 0);
    for how in WRITERS {
        let (ours, peer) = S::pair(false);
        write_all(&ours, UNREAD).await.unwrap();
        S::reset(peer);
        let what = format!("{}: a reset read, then a {how:?} write", S::NAME);
        let _ = within(&what, read_to_end_as(&ours, Reader::Borrowed, 4096)).await;
        let wrote = within(&what, write_as(&ours, how, &pattern(1, 100_000))).await;
        let expected = match (how, &want_file) {
            (Writer::SendFile, Some(file)) => file,
            _ => &want,
        };
        assert_eq!(Answer::of(&wrote, |_| 0), *expected, "{what}");
        let again = within(&what, write_as(&ours, Writer::Plain, b"more")).await;
        assert_eq!(
            Answer::of(&again, |_| 0),
            want,
            "{what}: the write after it"
        );
    }
}

/// A write far larger than the buffers, partly queued when the peer, having
/// read some of it, resets: the write fails as std's does, and so does the
/// next; the reads that follow end.
pub async fn reset_during_a_large_write<S: Stream>() {
    // Past what the kernel's buffers of both ends take at their largest.
    const LARGE: usize = 32 << 20;
    let bytes = pattern(3, LARGE);
    {
        let (mut std_ours, std_peer) = S::std_pair(false);
        let peer = std::thread::spawn(move || {
            let mut peer = std_peer;
            let mut buf = vec![0u8; 65_536];
            peer.read_exact(&mut buf).unwrap();
            S::reset(peer);
        });
        let wrote = std_ours.write_all(&bytes);
        joined(peer).await;
        // Windows may take the whole write into its buffers before the reset lands.
        if let Err(err) = wrote {
            assert!(
                is_one_of(&err, RESET_WRITE),
                "std's write past a reset: {err}"
            );
        } else if !cfg!(windows) {
            panic!("std's write past a reset went through");
        }
    }
    for how in WRITERS {
        let (ours, peer) = S::pair(false);
        let peer = std::thread::spawn(move || {
            let mut peer = peer;
            let mut buf = vec![0u8; 65_536];
            peer.read_exact(&mut buf).unwrap();
            S::reset(peer);
        });
        let what = format!(
            "{}: a {how:?} write of {LARGE} bytes, reset part way",
            S::NAME
        );
        let wrote = within(&what, write_as(&ours, how, &bytes)).await;
        joined(peer).await;
        match wrote {
            Err(err) => assert!(is_one_of(&err, RESET_WRITE), "{what}: {err}"),
            Ok(()) if cfg!(windows) => {}
            Ok(()) => panic!("{what}: the write went through"),
        }
        let again = within(&what, write_as(&ours, Writer::Plain, b"more")).await;
        let err = again.expect_err(&format!("{what}: the write after it"));
        assert!(
            is_one_of(&err, RESET_WRITE),
            "{what}: the write after it: {err}"
        );
        let read = within(&what, read_once(&ours, Reader::Borrowed, 4096)).await;
        match read {
            Ok(bytes) => assert!(bytes.is_empty(), "{what}: a read after it found bytes"),
            Err(err) => assert!(
                is_one_of(&err, RESET_READ),
                "{what}: the read after it: {err}"
            ),
        }
    }
}

/// The peer read every byte of a write, then resets: the next read and
/// write answer as std's.
pub async fn reset_after_a_write<S: Stream>() {
    let bytes = pattern(9, 20_000);
    let (mut std_ours, mut std_peer) = S::std_pair(false);
    std_ours.write_all(&bytes).unwrap();
    let mut buf = vec![0u8; bytes.len()];
    std_peer.read_exact(&mut buf).unwrap();
    std_ours.write_all(UNREAD).unwrap();
    S::reset(std_peer);
    let mut one = [0u8; 16];
    let want_read = Answer::of(&std_ours.read(&mut one), |n| *n);
    let want_write = Answer::of(&std_ours.write_all(b"more"), |_| 0);
    for how in WRITERS {
        let (ours, peer) = S::pair(false);
        let what = format!("{}: a {how:?} write the peer read, then a reset", S::NAME);
        let reader = std::thread::spawn(move || {
            let mut peer = peer;
            let mut buf = vec![0u8; 20_000];
            peer.read_exact(&mut buf).unwrap();
            (peer, buf)
        });
        within(&what, write_as(&ours, how, &bytes)).await.unwrap();
        let (peer, got) = joined(reader).await;
        same_bytes(&what, &got, &bytes);
        write_all(&ours, UNREAD).await.unwrap();
        S::reset(peer);
        let read = within(&what, read_once(&ours, Reader::Borrowed, 16)).await;
        assert_eq!(Answer::of(&read, Vec::len), want_read, "{what}: the read");
        let wrote = within(&what, write_as(&ours, Writer::Plain, b"more")).await;
        assert_eq!(Answer::of(&wrote, |_| 0), want_write, "{what}: the write");
    }
}

macro_rules! reset_cases {
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
            fn a_reset_before_a_read() {
                run(
                    concat!("reset::", stringify!($module), "::a_reset_before_a_read"),
                    super::reset_before_a_read::<$ty>,
                );
            }

            #[test]
            fn a_reset_during_a_read() {
                run(
                    concat!("reset::", stringify!($module), "::a_reset_during_a_read"),
                    super::reset_during_a_read::<$ty>,
                );
            }

            #[test]
            fn a_reset_after_a_read() {
                run(
                    concat!("reset::", stringify!($module), "::a_reset_after_a_read"),
                    super::reset_after_a_read::<$ty>,
                );
            }

            #[test]
            fn a_reset_then_a_write() {
                run(
                    concat!("reset::", stringify!($module), "::a_reset_then_a_write"),
                    super::reset_then_a_write::<$ty>,
                );
            }

            #[test]
            fn a_reset_during_a_large_write() {
                run(
                    concat!(
                        "reset::",
                        stringify!($module),
                        "::a_reset_during_a_large_write"
                    ),
                    super::reset_during_a_large_write::<$ty>,
                );
            }

            #[test]
            fn a_reset_after_a_write() {
                run(
                    concat!("reset::", stringify!($module), "::a_reset_after_a_write"),
                    super::reset_after_a_write::<$ty>,
                );
            }

            #[test]
            fn resets_hold_nothing() {
                let name = concat!("reset::", stringify!($module), "::resets_hold_nothing");
                if alone(name) {
                    holds_steady(name, 3, || async {
                        super::reset_during_a_read::<$ty>().await;
                        super::reset_during_a_large_write::<$ty>().await;
                    });
                }
            }
        }
    };
}

reset_cases!(tcp, nuclei::net::TcpStream);
#[cfg(unix)]
reset_cases!(unix, nuclei::net::UnixStream);

/// A connect to a port nobody listens on is refused, as std's; one to a
/// listener that resets the connection it accepts connects, and its read
/// answers the reset.
#[test]
fn connects_refused_and_reset_answer_as_std() {
    if !alone("reset::connects_refused_and_reset_answer_as_std") {
        return;
    }
    let rt = runtime();
    rt.block_on(async {
        let freed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = freed.local_addr().unwrap();
        drop(freed);
        let want = Answer::of(&std::net::TcpStream::connect(addr), |_| 0);
        let got = within("a refused connect", nuclei::net::TcpStream::connect(addr)).await;
        assert_eq!(Answer::of(&got, |_| 0), want, "a refused connect");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let resetter = std::thread::spawn(move || {
            for _ in 0..2 {
                let (conn, _) = listener.accept().unwrap();
                drop(zero_linger(conn));
            }
        });
        // The reset may come before the connect saw the handshake end (then
        // the connect answers it, as std's blocking connect does when it wakes
        // after the reset) or after (then the first read does).
        let answered =
            |connect: io::Result<()>, read: Option<io::Result<usize>>| match (connect, read) {
                (Err(err), None) => err.kind(),
                (Ok(()), Some(Err(err))) => err.kind(),
                other => panic!("a connection reset at once: {other:?}"),
            };
        let mut std_conn = std::net::TcpStream::connect(addr);
        let std_read = std_conn.as_mut().ok().map(|c| c.read(&mut [0u8; 8]));
        let want = answered(std_conn.map(drop), std_read);
        assert_eq!(
            want,
            io::ErrorKind::ConnectionReset,
            "std on a connection reset at once"
        );
        let conn = within("a connect reset", nuclei::net::TcpStream::connect(addr)).await;
        let read = match &conn {
            Ok(conn) => Some(within("a read reset", read_some(conn, &mut [0u8; 8])).await),
            Err(_) => None,
        };
        assert_eq!(
            answered(conn.map(drop), read),
            want,
            "a connection reset at once"
        );
        joined(resetter).await;
    });
    rt.block_on(round_trip());
}
