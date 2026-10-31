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
/// `a_listener_dropped_with_an_accept_out_is_closed_at_once`.
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
/// behind is `a_file_dropped_with_its_runtime_lands_its_writes`.
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
    let mut peers = Vec::new();
    // Returns the peer of the owned write, which never reads: the bytes its
    // handle left behind go out as the runtime drops, for a minute at most
    // (`a_runtime_dropped_with_bytes_behind_delivers_them`), so it resets
    // just before, and they end there.
    let busy = |rt: &nuclei::Runtime, peers: &mut Vec<Box<dyn Send>>| {
        let (ours, writes_to) = <TcpStream as Stream>::pair(true);
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
        writes_to
    };
    let before = Held::now();
    for _ in 0..3 {
        let rt = runtime();
        let writes_to = busy(&rt, &mut peers);
        rt.block_on(round_trip());
        <TcpStream as Stream>::reset(writes_to);
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

/// A listener dropped with an accept out is closed when the drop returns, as
/// std's is: a connect to its port is refused.
#[test]
fn a_listener_dropped_with_an_accept_out_is_closed_at_once() {
    if !alone("drops::a_listener_dropped_with_an_accept_out_is_closed_at_once") {
        return;
    }
    let rt = runtime();
    rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        poll_times("an accept", listener.accept(), 1, DropAt::Poll).await;
        // The accept reaches the kernel before the drop.
        nuclei::time::sleep(Duration::from_millis(20)).await;
        drop(listener);
        let refused = std::net::TcpStream::connect(addr);
        assert!(
            refused.is_err(),
            "a connect to a listener dropped with an accept out"
        );
    });
}

/// A stream dropped with a read out, by a thread that then blocks and runs
/// its shard no more, is closed all the same, as std's is: its peer reads
/// the end at once.
#[test]
fn a_stream_dropped_by_a_thread_that_then_blocks_is_closed() {
    if !alone("drops::a_stream_dropped_by_a_thread_that_then_blocks_is_closed") {
        return;
    }
    let rt = runtime();
    rt.block_on(async {
        let (ours, mut peer) = <TcpStream as Stream>::pair(false);
        let mut buf = [0u8; 64];
        poll_times("a read", read_some(&ours, &mut buf), 1, DropAt::Poll).await;
        // The read reaches the kernel before the drop.
        nuclei::time::sleep(Duration::from_millis(20)).await;
        drop(ours);
        // This thread blocks in the peer's read, its shard with it.
        let mut end = [0u8; 8];
        let got = peer.read(&mut end);
        assert_eq!(
            Answer::of(&got, |n| *n),
            Answer::Ok(0),
            "the peer of a stream dropped by a thread that then blocked"
        );
    });
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

/// What a case whose runtime's drop writes a file on this thread holds
/// before it: this thread's shard, and on io_uring the worker the kernel
/// keeps for its ring once a file's write went to it.
fn warm_up_with_a_write() {
    use nuclei::io::AsyncWriteExt;
    let warm = runtime();
    warm.block_on(round_trip());
    let ahead = TempFile::new("drop-runtime-ahead");
    warm.block_on(async {
        let mut handle = nuclei::fs::File::create(&ahead.0).await.unwrap();
        handle.write_all(&pattern(8, 64 * 1024)).await.unwrap();
        handle.flush().await.unwrap();
    });
    drop(warm);
}

/// A file whose task drops with its runtime, with writes accepted behind,
/// lands them before the drop returns, as std's writes land once they
/// returned, and leaves no descriptor open.
#[test]
fn a_file_dropped_with_its_runtime_lands_its_writes() {
    if !alone("drops::a_file_dropped_with_its_runtime_lands_its_writes") {
        return;
    }
    use nuclei::io::AsyncWriteExt;
    let file = TempFile::new("drop-runtime");
    warm_up_with_a_write();
    let before = Held::now();
    let bytes = pattern(9, 300_000);
    let rt = runtime();
    let (taken, held) = std::sync::mpsc::channel();
    let path = file.0.clone();
    let written = bytes.clone();
    rt.spawn(async move {
        let mut handle = nuclei::fs::File::create(&path).await.unwrap();
        handle.write_all(&written).await.unwrap();
        taken.send(()).unwrap();
        core::future::pending::<()>().await;
        drop(handle);
    })
    .detach();
    held.recv_timeout(DEADLINE).unwrap();
    drop(rt);
    same_bytes(
        "a file's writes when its runtime dropped",
        &std::fs::read(&file.0).unwrap(),
        &bytes,
    );
    eventually(
        "the file's descriptors and the runtime's threads to go",
        || {
            let now = Held::now();
            now.handles <= before.handles && now.threads <= before.threads
        },
    );
}

/// A task whose file holds a write behind one the blocking pool has not
/// made yet as its runtime drops is dropped only where that write ends, on
/// the pool's thread: the drop waits for it, and lands what the task's file
/// held, before it returns.
#[test]
fn a_file_whose_write_waits_for_the_pool_as_its_runtime_drops_lands_its_writes() {
    if !alone("drops::a_file_whose_write_waits_for_the_pool_as_its_runtime_drops_lands_its_writes")
    {
        return;
    }
    use nuclei::io::AsyncWriteExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let file = TempFile::new("drop-runtime-pool");
    warm_up_with_a_write();
    let before = Held::now();
    let stopped = Arc::new(AtomicUsize::new(0));
    let rt = nuclei::Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .max_blocking_threads(1)
        .thread_keep_alive(Duration::from_millis(100))
        .on_thread_stop({
            let stopped = Arc::clone(&stopped);
            move || {
                stopped.fetch_add(1, Ordering::SeqCst);
            }
        })
        .build()
        .unwrap();
    // The pool's one thread is held until both workers stopped: a write on
    // the pool waits for it meanwhile.
    let (running, holds) = std::sync::mpsc::channel();
    let held_pool = {
        let stopped = Arc::clone(&stopped);
        rt.spawn_blocking(move || {
            running.send(()).unwrap();
            eventually("both workers to stop", || {
                stopped.load(Ordering::SeqCst) >= 2
            });
        })
    };
    held_pool.detach();
    holds.recv_timeout(DEADLINE).unwrap();
    let bytes = pattern(11, 200_000);
    let opened = std::fs::File::create(&file.0).unwrap();
    let (taken, accepted) = std::sync::mpsc::channel();
    let written = bytes.clone();
    rt.spawn(async move {
        let mut handle = nuclei::Handle::new(opened).unwrap();
        // One write out (on the ring, or waiting for the pool) and one
        // behind it.
        let mut at = 0;
        for _ in 0..2 {
            at += handle.write(&written[at..]).await.unwrap();
        }
        taken.send(at).unwrap();
        core::future::pending::<()>().await;
        drop(handle);
    })
    .detach();
    let at = accepted.recv_timeout(DEADLINE).unwrap();
    drop(rt);
    same_bytes(
        "a file's writes when its runtime dropped with one waiting for the pool",
        &std::fs::read(&file.0).unwrap(),
        &bytes[..at],
    );
    eventually(
        "the file's descriptors and the runtime's threads to go",
        || {
            let now = Held::now();
            now.handles <= before.handles && now.threads <= before.threads
        },
    );
}

/// Signals as it drops: what holds it went.
struct Gone(std::sync::mpsc::Sender<()>);

impl Drop for Gone {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// A task that parks for good holding a file whose write is still out is
/// dropped once that write ends, file and all: its own registration on the
/// file keeps nothing alive.
#[test]
fn a_task_parked_holding_a_file_whose_write_is_out_goes_once_it_ends() {
    use nuclei::io::AsyncWriteExt;
    let file = TempFile::new("parked-holder");
    let rt = runtime();
    let (gone, dropped) = std::sync::mpsc::channel();
    let path = file.0.clone();
    let bytes = pattern(12, 4096);
    let written = bytes.clone();
    rt.spawn(async move {
        let _gone = Gone(gone);
        let mut handle = nuclei::fs::File::create(&path).await.unwrap();
        // Accepted at once, and still out as the task parks.
        assert_eq!(handle.write(&written).await.unwrap(), written.len());
        core::future::pending::<()>().await;
        drop(handle);
    })
    .detach();
    assert!(
        dropped.recv_timeout(DEADLINE).is_ok(),
        "a task parked holding a file outlived its write"
    );
    same_bytes(
        "the parked task's write",
        &std::fs::read(&file.0).unwrap(),
        &bytes,
    );
    drop(rt);
}

/// A stream whose task drops with its runtime, with bytes in its
/// write-behind, delivers them before its end, as std's do when the process
/// lets go of a socket with bytes unsent; no other runtime is started for
/// them, and nothing is left open or running after.
#[test]
fn a_stream_dropped_with_its_runtime_delivers_its_bytes_and_starts_no_runtime() {
    if !alone("drops::a_stream_dropped_with_its_runtime_delivers_its_bytes_and_starts_no_runtime") {
        return;
    }
    let warm = runtime();
    warm.block_on(round_trip());
    drop(warm);
    let before = Held::now();
    let bytes = pattern(10, 300_000);
    let rt = runtime();
    // Inside the runtime: on Windows the pair's small-buffered socket
    // connects through nuclei, which outside one would start the default.
    let (ours, peer) = rt.block_on(async { <TcpStream as Stream>::pair(true) });
    let reader = drain::<TcpStream>(peer);
    let (taken, held) = std::sync::mpsc::channel();
    let written = bytes.clone();
    rt.spawn(async move {
        let mut at = 0;
        while at < written.len() {
            match ours.try_write(&written[at..]) {
                Ok(n) => at += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("a write: {err}"),
            }
        }
        taken.send(at).unwrap();
        core::future::pending::<()>().await;
        drop(ours);
    })
    .detach();
    let at = held.recv_timeout(DEADLINE).unwrap();
    drop(rt);
    let (got, end) = reader.join().unwrap();
    assert!(end.is_ok(), "the peer's end: {end:?}");
    same_bytes(
        "bytes behind when their task's runtime dropped",
        &got,
        &bytes[..at],
    );
    eventually(
        "the stream's descriptors and the runtime's threads to go",
        || {
            let now = Held::now();
            now.handles <= before.handles && now.threads <= before.threads
        },
    );
}
