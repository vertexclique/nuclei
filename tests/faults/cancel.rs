//! Cancellation at every await: each call's future polled k times, for k
//! from 0 until it completes by itself, then dropped, right after its last
//! poll or after the wake that follows it (the driver did the work, the
//! future never saw it). Then the call's documented contract holds: an owned
//! write is on the wire whole or not at all, a plain write's taken bytes are
//! there and nothing else, a read loses nothing the next read should get, an
//! accept loses no connection, a datagram arrives whole or not at all, a
//! file's owned write lands and its owned read takes the cursor nowhere, a
//! file sent is a prefix on the wire; and the runtime works afterwards.

use core::future::poll_fn;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nuclei::net::{TcpListener, TcpStream, UdpSocket};

use crate::support::*;

const TRAILER: &[u8] = b"-- the end --";
const AT: [DropAt; 2] = [DropAt::Poll, DropAt::Wake];

/// A peer that reads everything until the end, on a thread of its own.
fn drain<S: Stream>(peer: S::Std) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut peer = peer;
        let (got, end) = std_read_until_end(&mut peer, 8192);
        assert!(end.is_ok(), "the peer's read: {end:?}");
        got
    })
}

pub async fn owned_write<S: Stream>() {
    for (size, tiny) in [(100, true), (70_000, true), (600_000, false)] {
        let bytes = pattern(size as u64, size);
        for at in AT {
            for k in 0.. {
                let what = format!(
                    "{}: an owned write of {size}, dropped at {at:?} after {k} polls",
                    S::NAME
                );
                let (ours, peer) = S::pair(tiny);
                let peer = drain::<S>(peer);
                let done = poll_times(&what, ours.write_owned(bytes.clone()), k, at).await;
                within(&what, write_all(&ours, TRAILER)).await.unwrap();
                within(&what, close(&ours)).await.unwrap();
                let got = joined(peer).await;
                let whole = [bytes.as_slice(), TRAILER].concat();
                match &done {
                    Some((result, back)) => {
                        assert_eq!(result.as_ref().ok(), Some(&size), "{what}: {result:?}");
                        assert_eq!(back.len(), size, "{what}: the buffer back");
                        same_bytes(&what, &got, &whole);
                    }
                    None => assert!(
                        got == whole || got == TRAILER,
                        "{what}: {} bytes on the wire, neither the write whole nor none of it",
                        got.len()
                    ),
                }
                if done.is_some() {
                    break;
                }
            }
        }
    }
}

/// Plain writes, each answering what the write-behind took: dropped, the
/// bytes taken are on the wire, and nothing past them.
pub async fn plain_write<S: Stream>(vectored: bool) {
    for (size, tiny) in [(70_000, true), (600_000, false)] {
        let bytes = pattern(size as u64 + 1, size);
        for at in AT {
            for k in 0.. {
                let what = format!(
                    "{}: plain writes of {size}, vectored {vectored}, dropped at {at:?} after {k} polls",
                    S::NAME
                );
                let (ours, peer) = S::pair(tiny);
                let peer = drain::<S>(peer);
                let taken = AtomicUsize::new(0);
                let writes = async {
                    while taken.load(Ordering::Relaxed) < size {
                        let rest = &bytes[taken.load(Ordering::Relaxed)..];
                        let n = if vectored {
                            let (a, b) = rest.split_at(rest.len() / 2);
                            let bufs = [std::io::IoSlice::new(a), std::io::IoSlice::new(b)];
                            poll_fn(|cx| ours.poll_write_vectored(cx, &bufs)).await
                        } else {
                            write_some(&ours, rest).await
                        };
                        taken.fetch_add(n.unwrap(), Ordering::Relaxed);
                    }
                };
                let done = poll_times(&what, writes, k, at).await;
                let taken = taken.load(Ordering::Relaxed);
                within(&what, write_all(&ours, TRAILER)).await.unwrap();
                within(&what, close(&ours)).await.unwrap();
                let got = joined(peer).await;
                same_bytes(&what, &got, &[&bytes[..taken], TRAILER].concat());
                if done.is_some() {
                    break;
                }
            }
        }
    }
}

/// Reads of `size` bytes the peer sends: dropped, what they took is theirs,
/// and the next reads get the rest, in order.
pub async fn read<S: Stream>(owned: bool) {
    let size = 70_000;
    let bytes = pattern(77, size);
    for at in AT {
        for k in 0.. {
            let what = format!(
                "{}: reads, owned {owned}, dropped at {at:?} after {k} polls",
                S::NAME
            );
            let (ours, peer) = S::pair(true);
            let sent = bytes.clone();
            let writer = std::thread::spawn(move || {
                let mut peer = peer;
                peer.write_all(&sent).unwrap();
                S::shutdown_std(&peer, std::net::Shutdown::Write).unwrap();
                peer
            });
            let mut taken = Vec::new();
            let reads = async {
                loop {
                    if owned {
                        let (n, buf) = ours.read_owned(Vec::with_capacity(9000)).await;
                        if n.unwrap() == 0 {
                            return;
                        }
                        taken.extend_from_slice(&buf);
                    } else {
                        let mut buf = [0u8; 9000];
                        match read_some(&ours, &mut buf).await.unwrap() {
                            0 => return,
                            n => taken.extend_from_slice(&buf[..n]),
                        }
                    }
                }
            };
            let done = poll_times(&what, reads, k, at).await;
            let (rest, end) = within(&what, read_owned_until_end(&ours, 4096)).await;
            assert!(end.is_ok(), "{what}: {end:?}");
            drop(joined(writer).await);
            same_bytes(&what, &[taken, rest].concat(), &bytes);
            if done.is_some() {
                break;
            }
        }
    }
}

pub async fn send_file<S: Stream>() {
    let size = (3 << 20) + 5;
    let bytes = pattern(99, size);
    let file = TempFile::with("cancel-send-file", &bytes);
    let opened = nuclei::fs::File::open(&file.0).await.unwrap();
    for at in AT {
        for k in 0.. {
            let what = format!(
                "{}: send_file of {size}, dropped at {at:?} after {k} polls",
                S::NAME
            );
            let (ours, peer) = S::pair(false);
            let peer = drain::<S>(peer);
            let done = poll_times(&what, ours.send_file(&opened, 0, size as u64), k, at).await;
            drop(ours);
            let got = joined(peer).await;
            match done {
                Some(sent) => {
                    assert_eq!(sent.unwrap(), size as u64, "{what}");
                    same_bytes(&what, &got, &bytes);
                    break;
                }
                None => same_bytes(&what, &got, &bytes[..got.len().min(size)]),
            }
        }
    }
}

macro_rules! cancel_cases {
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
            fn an_owned_write_dropped_is_whole_or_absent() {
                run(
                    concat!(
                        "cancel::",
                        stringify!($module),
                        "::an_owned_write_dropped_is_whole_or_absent"
                    ),
                    super::owned_write::<$ty>,
                );
            }

            #[test]
            fn plain_writes_dropped_leave_what_they_took() {
                run(
                    concat!(
                        "cancel::",
                        stringify!($module),
                        "::plain_writes_dropped_leave_what_they_took"
                    ),
                    async || super::plain_write::<$ty>(false).await,
                );
            }

            #[test]
            fn vectored_writes_dropped_leave_what_they_took() {
                run(
                    concat!(
                        "cancel::",
                        stringify!($module),
                        "::vectored_writes_dropped_leave_what_they_took"
                    ),
                    async || super::plain_write::<$ty>(true).await,
                );
            }

            #[test]
            fn reads_dropped_lose_nothing() {
                run(
                    concat!(
                        "cancel::",
                        stringify!($module),
                        "::reads_dropped_lose_nothing"
                    ),
                    async || super::read::<$ty>(false).await,
                );
            }

            #[test]
            fn owned_reads_dropped_lose_nothing() {
                run(
                    concat!(
                        "cancel::",
                        stringify!($module),
                        "::owned_reads_dropped_lose_nothing"
                    ),
                    async || super::read::<$ty>(true).await,
                );
            }

            #[test]
            fn send_file_dropped_leaves_a_prefix() {
                run(
                    concat!(
                        "cancel::",
                        stringify!($module),
                        "::send_file_dropped_leaves_a_prefix"
                    ),
                    super::send_file::<$ty>,
                );
            }

            #[test]
            fn cancellations_hold_nothing() {
                let name = concat!(
                    "cancel::",
                    stringify!($module),
                    "::cancellations_hold_nothing"
                );
                if alone(name) {
                    holds_steady(name, 2, || async {
                        super::owned_write::<$ty>().await;
                        super::read::<$ty>(true).await;
                    });
                }
            }
        }
    };
}

cancel_cases!(tcp, nuclei::net::TcpStream);
#[cfg(unix)]
cancel_cases!(unix, nuclei::net::UnixStream);

/// Accepts dropped at every point: every connection made is accepted by a
/// later accept, none lost.
async fn accepts() {
    for at in AT {
        for k in 0..6 {
            let what = format!("an accept dropped at {at:?} after {k} polls");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let clients = std::thread::spawn(move || {
                (0..3)
                    .map(|_| std::net::TcpStream::connect(addr).unwrap())
                    .collect::<Vec<_>>()
            });
            let done = poll_times(&what, listener.accept(), k, at).await;
            let mut accepted = usize::from(done.is_some());
            if let Some(conn) = done {
                conn.unwrap();
            }
            while accepted < 3 {
                within(&what, listener.accept()).await.unwrap();
                accepted += 1;
            }
            drop(joined(clients).await);
        }
    }
    for at in AT {
        for k in 0..6 {
            let what = format!("the accept stream's next dropped at {at:?} after {k} polls");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let mut incoming = listener.accept_multi().await.unwrap();
            let clients = std::thread::spawn(move || {
                (0..3)
                    .map(|_| std::net::TcpStream::connect(addr).unwrap())
                    .collect::<Vec<_>>()
            });
            let next = poll_fn(|cx| {
                futures_core::Stream::poll_next(core::pin::Pin::new(&mut incoming), cx)
            });
            let done = poll_times(&what, next, k, at).await;
            let mut accepted = usize::from(done.is_some());
            while accepted < 3 {
                let conn = within(
                    &what,
                    poll_fn(|cx| {
                        futures_core::Stream::poll_next(core::pin::Pin::new(&mut incoming), cx)
                    }),
                )
                .await;
                assert!(
                    conn.is_some(),
                    "{what}: the stream ended: {:?}",
                    incoming.take_error()
                );
                accepted += 1;
            }
            drop(joined(clients).await);
        }
    }
}

/// Connects dropped at every point: no descriptor stays (the case counts
/// them), and a connect after works.
async fn connects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    for at in AT {
        for k in 0..4 {
            let what = format!("a connect dropped at {at:?} after {k} polls");
            let done = poll_times(&what, TcpStream::connect(addr), k, at).await;
            if let Some(conn) = done {
                conn.unwrap();
            }
        }
    }
    let (client, server) = tcp_pair().await;
    write_all(&client, b"x").await.unwrap();
    assert_eq!(read_exact(&server, 1).await.unwrap(), b"x");
}

/// Datagram sends and receives dropped at every point: a datagram arrives
/// whole or not at all, and none received is lost.
async fn datagrams() {
    for at in AT {
        for (size, owned) in [(100, false), (100, true), (30_000, true)] {
            for k in 0..5 {
                let what =
                    format!("a {size}-byte send, owned {owned}, dropped at {at:?} after {k} polls");
                let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let to = b.local_addr().unwrap();
                let bytes = pattern(size as u64, size);
                let done = if owned {
                    poll_times(&what, a.send_to_owned(bytes.clone(), to), k, at)
                        .await
                        .map(|r| r.0)
                } else {
                    poll_times(&what, a.send_to(&bytes, to), k, at).await
                };
                within(&what, a.send_to(TRAILER, to)).await.unwrap();
                let mut buf = vec![0u8; 65_536];
                let (n, _) = within(&what, b.recv_from(&mut buf)).await.unwrap();
                if n == TRAILER.len() && &buf[..n] == TRAILER {
                    assert!(done.is_none(), "{what}: sent, yet only the trailer came");
                } else {
                    same_bytes(&what, &buf[..n], &bytes);
                    let (n, _) = within(&what, b.recv_from(&mut buf)).await.unwrap();
                    same_bytes(&what, &buf[..n], TRAILER);
                }
            }
        }
        for owned in [false, true] {
            for k in 0..5 {
                let what = format!("a receive, owned {owned}, dropped at {at:?} after {k} polls");
                let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
                let to = b.local_addr().unwrap();
                let sender = nuclei::spawn(async move {
                    nuclei::task::yield_now().await;
                    a.send_to(b"datagram", to).await.unwrap();
                    a
                });
                let done = if owned {
                    poll_times(&what, b.recv_from_owned(Vec::with_capacity(64)), k, at)
                        .await
                        .map(|(r, buf)| r.map(|_| buf))
                } else {
                    let mut buf = vec![0u8; 64];
                    poll_times(&what, b.recv_from(&mut buf), k, at)
                        .await
                        .map(|r| r.map(|(n, _)| buf[..n].to_vec()))
                };
                let a = within(&what, sender).await;
                let got = match done {
                    Some(got) => got.unwrap(),
                    None => {
                        let mut buf = vec![0u8; 64];
                        let (n, _) = within(&what, b.recv_from(&mut buf)).await.unwrap();
                        buf[..n].to_vec()
                    }
                };
                assert_eq!(got, b"datagram", "{what}");
                a.send_to(b"next", to).await.unwrap();
                let mut buf = vec![0u8; 64];
                let (n, _) = within(&what, b.recv_from(&mut buf)).await.unwrap();
                assert_eq!(&buf[..n], b"next", "{what}: the next datagram");
            }
        }
    }
}

/// A file's owned write dropped lands whole; its owned read dropped takes
/// the cursor nowhere; its plain reads and writes dropped keep the bytes.
async fn files() {
    use nuclei::io::{AsyncReadExt, AsyncWriteExt};
    let bytes = pattern(5, 300_000);
    for at in AT {
        for k in 0..4 {
            let what = format!("a file's owned write dropped at {at:?} after {k} polls");
            let file = TempFile::with("cancel-file-write", b"");
            let mut handle = nuclei::fs::OpenOptions::new()
                .write(true)
                .open(&file.0)
                .await
                .unwrap();
            let done = poll_times(&what, handle.write_owned(bytes.clone()), k, at).await;
            if let Some((n, _)) = done {
                assert_eq!(n.unwrap(), bytes.len(), "{what}");
            }
            within(&what, handle.flush()).await.unwrap();
            if k > 0 {
                same_bytes(&what, &std::fs::read(&file.0).unwrap(), &bytes);
            } else {
                // Never polled, the write never started.
                assert!(std::fs::read(&file.0).unwrap().is_empty(), "{what}");
            }
        }
        for k in 0..4 {
            let what = format!("a file's owned read dropped at {at:?} after {k} polls");
            let file = TempFile::with("cancel-file-read", &bytes);
            let mut handle = nuclei::fs::File::open(&file.0).await.unwrap();
            let done = poll_times(&what, handle.read_owned(Vec::with_capacity(1000)), k, at).await;
            let mut taken = match done {
                Some((n, buf)) => {
                    assert_eq!(n.unwrap(), buf.len(), "{what}");
                    buf
                }
                None => Vec::new(),
            };
            within(&what, handle.read_to_end(&mut taken)).await.unwrap();
            same_bytes(&what, &taken, &bytes);
        }
        for k in 0..4 {
            let what = format!("a file's read_exact dropped at {at:?} after {k} polls");
            let file = TempFile::with("cancel-file-read-exact", &bytes);
            let mut handle = nuclei::fs::File::open(&file.0).await.unwrap();
            let mut first = vec![0u8; 100_000];
            let taken = Arc::new(AtomicUsize::new(0));
            let reads = async {
                while taken.load(Ordering::Relaxed) < first.len() {
                    let at = taken.load(Ordering::Relaxed);
                    let n = handle.read(&mut first[at..]).await.unwrap();
                    taken.fetch_add(n, Ordering::Relaxed);
                }
            };
            poll_times(&what, reads, k, at).await;
            let taken = taken.load(Ordering::Relaxed);
            let mut rest = Vec::new();
            within(&what, handle.read_to_end(&mut rest)).await.unwrap();
            same_bytes(&what, &[&first[..taken], &rest].concat(), &bytes);
        }
    }
}

fn run(name: &str, case: impl AsyncFnOnce()) {
    if alone(name) {
        let rt = runtime();
        rt.block_on(case());
        rt.block_on(round_trip());
    }
}

#[test]
fn accepts_dropped_lose_no_connection() {
    run("cancel::accepts_dropped_lose_no_connection", accepts);
}

#[test]
fn connects_dropped_leave_nothing() {
    let name = "cancel::connects_dropped_leave_nothing";
    if alone(name) {
        holds_steady(name, 3, connects);
    }
}

#[test]
fn datagram_calls_dropped_are_whole_or_absent_and_lose_nothing() {
    run(
        "cancel::datagram_calls_dropped_are_whole_or_absent_and_lose_nothing",
        datagrams,
    );
}

#[test]
fn file_calls_dropped_keep_their_contracts() {
    run("cancel::file_calls_dropped_keep_their_contracts", files);
}

#[test]
fn other_cancellations_hold_nothing() {
    let name = "cancel::other_cancellations_hold_nothing";
    if alone(name) {
        holds_steady(name, 2, || async {
            accepts().await;
            datagrams().await;
            files().await;
        });
    }
}
