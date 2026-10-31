//! Drops at the worst moments: a handle dropped with an operation out or
//! bytes behind, a runtime dropped with tasks, operations and timers out, a
//! shard that goes while its sources hold bytes read ahead. Bytes accepted
//! reach the peer, as std's do when its socket closes; nothing stays open or
//! held; and the runtime (or the next one) works afterwards.

use std::io::{self, Read, Write};
use std::time::Duration;

use nuclei::net::{TcpListener, TcpStream, UdpSocket};

use crate::support::*;

/// The peer of `ours` reading until the end, on a thread of its own.
fn drain<S: Stream>(peer: S::Std) -> std::thread::JoinHandle<(Vec<u8>, io::Result<usize>)> {
    std::thread::spawn(move || {
        let mut peer = peer;
        std_read_until_end(&mut peer, 65_536)
    })
}

/// Bytes the write-behind took, then the handle dropped: every one reaches
/// the peer before its end.
async fn dropped_with_bytes_behind<S: Stream>() {
    for tiny in [false, true] {
        let bytes = pattern(1, 3 << 20);
        let (ours, peer) = S::pair(tiny);
        // As much as the socket and then the write-behind take.
        let mut at = 0;
        while at < bytes.len() {
            match ours.try_write(&bytes[at..]) {
                Ok(n) => at += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("a write: {err}"),
            }
        }
        drop(ours);
        let (got, end) = joined(drain::<S>(peer)).await;
        assert!(end.is_ok(), "{}: the peer's end: {end:?}", S::NAME);
        same_bytes(
            &format!("{}: dropped with bytes behind, tiny {tiny}", S::NAME),
            &got,
            &bytes[..at],
        );
    }
}

/// An owned write out when its future and then its handle drop: the peer
/// gets it whole or not at all, then the end.
async fn dropped_with_an_owned_write_out<S: Stream>() {
    // Many times what small buffers hold, and few enough seconds at their pace.
    let bytes = pattern(2, 200_000);
    for k in 1..5 {
        let (ours, peer) = S::pair(true);
        let reader = drain::<S>(peer);
        poll_times(
            "an owned write",
            ours.write_owned(bytes.clone()),
            k,
            DropAt::Poll,
        )
        .await;
        drop(ours);
        let (got, end) = joined(reader).await;
        assert!(end.is_ok(), "{}: the peer's end: {end:?}", S::NAME);
        assert!(
            got.is_empty() || got == bytes,
            "{}: {} bytes of an owned write out when its handle dropped, after {k} polls",
            S::NAME,
            got.len()
        );
    }
}

/// A read out (on a ring, a receive in the kernel) when the handle drops:
/// the peer sees the end, and nothing stays.
async fn dropped_with_a_read_out<S: Stream>() {
    for owned in [false, true] {
        let (ours, peer) = S::pair(false);
        if owned {
            poll_times(
                "an owned read",
                ours.read_owned(Vec::with_capacity(4096)),
                1,
                DropAt::Poll,
            )
            .await;
        } else {
            let mut buf = [0u8; 4096];
            poll_times("a read", read_some(&ours, &mut buf), 1, DropAt::Poll).await;
        }
        drop(ours);
        let (got, end) = joined(drain::<S>(peer)).await;
        assert!(
            got.is_empty() && end.is_ok(),
            "{}: the peer's read: {end:?}",
            S::NAME
        );
    }
}

/// A listener and datagram sockets dropped with calls out hold nothing. That
/// the listener is closed when its drop returns is
/// `open::a_listener_dropped_with_an_accept_out_is_closed_at_once`.
async fn listener_and_datagrams_dropped_with_calls_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    poll_times("an accept", listener.accept(), 1, DropAt::Poll).await;
    drop(listener);
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buf = [0u8; 64];
    poll_times("a receive", socket.recv_from(&mut buf), 1, DropAt::Poll).await;
    drop(socket);
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    poll_times(
        "an owned receive",
        socket.recv_from_owned(Vec::with_capacity(64)),
        1,
        DropAt::Poll,
    )
    .await;
    drop(socket);
}

/// A file dropped with writes accepted and not landed: they land.
async fn file_dropped_with_writes_behind() {
    use nuclei::io::AsyncWriteExt;
    let bytes = pattern(3, 500_000);
    let file = TempFile::with("drop-file", b"");
    let mut handle = nuclei::fs::OpenOptions::new()
        .write(true)
        .open(&file.0)
        .await
        .unwrap();
    for piece in bytes.chunks(7000) {
        handle.write_all(piece).await.unwrap();
    }
    drop(handle);
    until("the writes of a dropped file to land", || {
        std::fs::read(&file.0).is_ok_and(|got| got.len() == bytes.len())
    })
    .await;
    same_bytes(
        "a dropped file's writes",
        &std::fs::read(&file.0).unwrap(),
        &bytes,
    );
}

macro_rules! drop_cases {
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
            fn a_handle_dropped_with_bytes_behind_delivers_them() {
                run(
                    concat!(
                        "drops::",
                        stringify!($module),
                        "::a_handle_dropped_with_bytes_behind_delivers_them"
                    ),
                    super::dropped_with_bytes_behind::<$ty>,
                );
            }

            #[test]
            fn a_handle_dropped_with_an_owned_write_out_delivers_it_whole_or_not_at_all() {
                run(
                    concat!(
                        "drops::",
                        stringify!($module),
                        "::a_handle_dropped_with_an_owned_write_out_delivers_it_whole_or_not_at_all"
                    ),
                    super::dropped_with_an_owned_write_out::<$ty>,
                );
            }

            #[test]
            fn a_handle_dropped_with_a_read_out_ends_the_stream() {
                run(
                    concat!(
                        "drops::",
                        stringify!($module),
                        "::a_handle_dropped_with_a_read_out_ends_the_stream"
                    ),
                    super::dropped_with_a_read_out::<$ty>,
                );
            }

            #[test]
            fn handles_dropped_with_calls_out_hold_nothing() {
                let name = concat!(
                    "drops::",
                    stringify!($module),
                    "::handles_dropped_with_calls_out_hold_nothing"
                );
                if alone(name) {
                    holds_steady(name, 3, || async {
                        super::dropped_with_bytes_behind::<$ty>().await;
                        super::dropped_with_an_owned_write_out::<$ty>().await;
                        super::dropped_with_a_read_out::<$ty>().await;
                    });
                }
            }
        }
    };
}

drop_cases!(tcp, nuclei::net::TcpStream);
#[cfg(unix)]
drop_cases!(unix, nuclei::net::UnixStream);

#[test]
fn listeners_and_datagram_sockets_dropped_with_calls_out_hold_nothing() {
    let name = "drops::listeners_and_datagram_sockets_dropped_with_calls_out_hold_nothing";
    if alone(name) {
        holds_steady(name, 5, listener_and_datagrams_dropped_with_calls_out);
    }
}

#[test]
fn a_file_dropped_with_writes_behind_lands_them() {
    let name = "drops::a_file_dropped_with_writes_behind_lands_them";
    if alone(name) {
        holds_steady(name, 3, file_dropped_with_writes_behind);
    }
}

/// Everything a runtime can have out when it drops: tasks reading, writing
/// owned buffers a peer never takes, accepting, receiving, sleeping, waiting
/// on the blocking pool. Once it is gone the process holds no more than
/// before it was built, and a runtime after it works. A file with writes
/// behind is open::a_file_dropped_with_its_runtime_lands_its_writes.
#[test]
fn a_runtime_dropped_with_everything_out_holds_nothing() {
    if !alone("drops::a_runtime_dropped_with_everything_out_holds_nothing") {
        return;
    }
    let warm = || {
        let rt = runtime();
        rt.block_on(round_trip());
        drop(rt);
    };
    warm();
    drop(<TcpStream as Stream>::pair(true));
    // A handle dropped with its runtime, with bytes or file writes behind,
    // hands them to a task of the process default runtime, which starts if
    // it is not running: it runs from before the count, for good.
    nuclei::block_on(nuclei::spawn(async {}));
    let mut peers = Vec::new();
    let busy = |rt: &nuclei::Runtime, peers: &mut Vec<Box<dyn Send>>| {
        let (ours, peer) = <TcpStream as Stream>::pair(true);
        peers.push(Box::new(peer));
        // Below the zero-copy size: a zero-copy send out when its runtime
        // drops is open::a_runtime_dropped_with_a_zero_copy_send_out_leaves_no_descriptor_open.
        rt.spawn(async move {
            let _ = ours.write_owned(vec![1u8; 512 << 10]).await;
        })
        .detach();
        let (ours, peer) = <TcpStream as Stream>::pair(false);
        peers.push(Box::new(peer));
        rt.spawn(async move {
            let _ = read_some(&ours, &mut [0u8; 4096]).await;
        })
        .detach();
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        rt.spawn(async move {
            let _ = listener.accept().await;
        })
        .detach();
        let socket = rt.block_on(UdpSocket::bind("127.0.0.1:0")).unwrap();
        rt.spawn(async move {
            let _ = socket.recv_from_owned(Vec::with_capacity(64)).await;
        })
        .detach();
        rt.spawn(nuclei::time::sleep(Duration::from_secs(3600)))
            .detach();
        let (release, held) = std::sync::mpsc::channel::<()>();
        peers.push(Box::new(release));
        rt.spawn(async move {
            let _ = nuclei::spawn_blocking(move || held.recv_timeout(DEADLINE)).await;
        })
        .detach();
        rt.block_on(nuclei::task::yield_now());
    };
    let before = Held::now();
    for _ in 0..3 {
        // No handoff: a worker the busy host stops past the threshold would
        // be handed off, and a runtime dropped after a handoff leaks its
        // pollers (open::a_runtime_dropped_after_a_handoff_leaves_no_descriptor_open).
        let rt = nuclei::Runtime::builder()
            .worker_threads(2)
            .pin_workers(false)
            .thread_keep_alive(Duration::from_millis(100))
            .handoff_after(None)
            .build()
            .unwrap();
        busy(&rt, &mut peers);
        rt.block_on(round_trip());
        drop(rt);
        peers.clear();
    }
    let mut after = Held::now();
    let deadline = std::time::Instant::now() + DEADLINE;
    while (after.handles > before.handles || after.threads > before.threads)
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
        after = Held::now();
    }
    assert!(
        after.handles <= before.handles,
        "{} descriptors open before the runtimes, {} after they dropped",
        before.handles,
        after.handles
    );
    assert!(
        after.threads <= before.threads,
        "{:?} threads before the runtimes, {:?} after they dropped",
        before.threads,
        after.threads
    );
    let rt = runtime();
    rt.block_on(round_trip());
}

/// Bytes the write-behind took when its runtime drops reach the peer, as
/// std's do when its process lets go of a socket with bytes unsent.
#[test]
fn a_runtime_dropped_with_bytes_behind_delivers_them() {
    if !alone("drops::a_runtime_dropped_with_bytes_behind_delivers_them") {
        return;
    }
    let bytes = pattern(4, 300_000);
    let rt = runtime();
    let (ours, peer) = <TcpStream as Stream>::pair(true);
    let reader = drain::<TcpStream>(peer);
    let at = rt.block_on(async {
        let mut at = 0;
        while at < bytes.len() {
            match ours.try_write(&bytes[at..]) {
                Ok(n) => at += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("a write: {err}"),
            }
        }
        at
    });
    drop(ours);
    drop(rt);
    let (got, end) = reader.join().unwrap();
    assert!(end.is_ok(), "the peer's end: {end:?}");
    same_bytes("bytes behind when the runtime dropped", &got, &bytes[..at]);
}

/// A shard that goes while a stream of it holds bytes it read ahead: the
/// stream's next reads on another shard get them, in order, and its writes
/// go on.
#[test]
fn a_shard_that_goes_hands_its_streams_on_with_every_byte() {
    if !alone("drops::a_shard_that_goes_hands_its_streams_on_with_every_byte") {
        return;
    }
    let rt = runtime();
    let bytes = pattern(6, 200_000);
    rt.block_on(async {
        for round in 0..4 {
            assert_eq!(
                within("a worker more", nuclei::spawn_more_threads(1))
                    .await
                    .unwrap(),
                1
            );
            let (ours, mut peer) = <TcpStream as Stream>::pair(false);
            peer.write_all(&bytes).unwrap();
            let ours = std::sync::Arc::new(ours);
            let first = {
                let ours = std::sync::Arc::clone(&ours);
                nuclei::spawn(async move {
                    let got = read_exact(&*ours, 10).await.unwrap();
                    // Waits for the stop: the shard takes no task after it.
                    let stopped = nuclei::stop_current_thread().await;
                    (got, stopped)
                })
            };
            let (mut got, stopped) = within("the first read", first).await;
            let what = format!("round {round}: a stream whose shard went (stopped {stopped})");
            peer.write_all(b"after").unwrap();
            let rest = within(&what, read_exact(&*ours, bytes.len() - 10 + 5)).await;
            got.extend_from_slice(&rest.unwrap());
            same_bytes(&what, &got, &[bytes.as_slice(), b"after"].concat());
            within(&what, write_all(&*ours, b"back")).await.unwrap();
            let mut four = [0u8; 4];
            peer.read_exact(&mut four).unwrap();
            assert_eq!(&four, b"back", "{what}");
        }
    });
    rt.block_on(round_trip());
}


/// A runtime dropped after one of its workers was handed off (blocked past
/// `handoff_after`), with tasks waiting on timers, leaves no descriptor open:
/// every shard's poller goes with it.
#[test]
fn a_runtime_dropped_after_a_handoff_leaves_no_descriptor_open() {
    if !alone("drops::a_runtime_dropped_after_a_handoff_leaves_no_descriptor_open") {
        return;
    }
    let warm = runtime();
    warm.block_on(round_trip());
    drop(warm);
    let before = Held::now();
    for _ in 0..4 {
        let rt = nuclei::Runtime::builder()
            .worker_threads(2)
            .pin_workers(false)
            .handoff_after(Some(Duration::from_millis(5)))
            .build()
            .unwrap();
        for _ in 0..4 {
            rt.spawn(async {
                std::thread::sleep(Duration::from_millis(30));
                nuclei::time::sleep(Duration::from_secs(3600)).await;
            })
            .detach();
        }
        rt.block_on(nuclei::time::sleep(Duration::from_millis(50)));
        drop(rt);
    }
    eventually("the dropped runtimes' descriptors to close", || {
        Held::now().handles <= before.handles
    });
}
