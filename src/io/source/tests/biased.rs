//! Sources on real sockets whose sides are biased to the shard that homes
//! them: a thread that is not that shard's runner switches a side to shared
//! mode and keeps every byte whole and in order, also when the shard itself
//! moves to another thread mid-read and mid-write, and when an owner and
//! two foreign threads poll one socket at once.

use super::*;

fn fenced() -> bool {
    crate::rt::handoff::fence::available()
}

/// One round trip through both sockets' sources on the runtime's worker,
/// which homes them: the worker is their owner from now on.
fn home_on_worker(rt: &Runtime, a: &Owned<Stream>, b: &Owned<Stream>) {
    let (a, b) = (Arc::clone(&a.src), Arc::clone(&b.src));
    rt.block_on(rt.spawn(async move {
        write_all(&a, b"x").await.unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(within(read(&b, &mut buf)).await.unwrap(), 1);
    }));
}

#[test]
fn a_thread_that_is_not_the_owner_switches_the_side_and_the_bytes_arrive_whole_and_in_order() {
    let rt = runtime(None);
    let (a, b) = pair();
    home_on_worker(&rt, &a, &b);
    if fenced() {
        assert!(!a.src.shared().1, "the owner's write side left bias");
        assert!(!b.src.shared().0, "the owner's read side left bias");
    }
    let data = pattern(1 << 20);
    let handle = rt.handle();
    let writer = {
        let (src, data) = (Arc::clone(&a.src), data.clone());
        let handle = handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                write_all(&src, &data).await.unwrap();
                flush(&src).await.unwrap();
            })
        })
    };
    let got = rt.block_on(async {
        let mut got = vec![0u8; data.len()];
        within(read_exact(&b.src, &mut got)).await.unwrap();
        got
    });
    writer.join().unwrap();
    assert!(got == data, "the bytes were damaged or misordered");
    if fenced() {
        assert!(a.src.shared().1, "a foreign write left the side biased");
        assert!(b.src.shared().0, "a foreign read left the side biased");
    }
}

/// A read pending on the worker, then a thread that is not the owner polls
/// the same side: the side goes shared and the worker's read still gets the
/// bytes that come.
#[test]
fn a_foreign_poll_during_the_owners_read_leaves_the_read_whole() {
    let rt = runtime(None);
    let (a, b) = pair();
    home_on_worker(&rt, &a, &b);
    let data = pattern(256 * 1024);
    let pending = Arc::new(AtomicBool::new(false));
    let reader = {
        let (src, seen) = (Arc::clone(&b.src), Arc::clone(&pending));
        rt.spawn(async move {
            let mut got = vec![0u8; 256 * 1024];
            let mut at = 0;
            while at < got.len() {
                let n = within(poll_fn(|cx| {
                    let polled = src.poll_read(cx, &mut got[at..], false);
                    if polled.is_pending() {
                        seen.store(true, Ordering::SeqCst);
                    }
                    polled
                }))
                .await
                .unwrap();
                at += n;
            }
            got
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pending.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the read never waited");
        std::thread::yield_now();
    }
    // A thread that runs no shard polls the read side, with its own waker.
    let woke = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&woke));
    let mut cx = Context::from_waker(&waker);
    let mut spare = [0u8; 8];
    let first = b.src.poll_read(&mut cx, &mut spare, true);
    if fenced() {
        assert!(b.src.shared().0, "the foreign poll left the side biased");
    }
    send(a.io(), &data);
    let got = rt.block_on(reader);
    assert!(got == data, "the read lost or reordered bytes");
    drop(first);
}

/// A write pending on the worker, a foreign flush on the same side, then
/// the peer drains: every byte arrives whole and in order.
#[test]
fn a_foreign_poll_during_the_owners_write_leaves_the_write_whole() {
    let rt = runtime(None);
    let (a, b) = pair();
    home_on_worker(&rt, &a, &b);
    // The socket takes no more before the write starts, so the write cannot
    // go straight to it: it is left to the driver, and the task waits.
    let filled = fill(a.io());
    let data = pattern(4 << 20);
    let pending = Arc::new(AtomicBool::new(false));
    let writer = {
        let (src, seen, data) = (Arc::clone(&a.src), Arc::clone(&pending), data.clone());
        rt.spawn(async move {
            let mut at = 0;
            while at < data.len() {
                let n = within(poll_fn(|cx| {
                    let polled = src.poll_write(cx, &data[at..]);
                    if polled.is_pending() {
                        seen.store(true, Ordering::SeqCst);
                    }
                    polled
                }))
                .await
                .unwrap();
                at += n;
            }
            within(poll_fn(|cx| {
                let polled = src.poll_flush(cx);
                if polled.is_pending() {
                    seen.store(true, Ordering::SeqCst);
                }
                polled
            }))
            .await
            .unwrap();
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pending.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the write never waited");
        std::thread::yield_now();
    }
    let woke = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&woke));
    let mut cx = Context::from_waker(&waker);
    assert!(a.src.poll_flush(&mut cx).is_pending());
    if fenced() {
        assert!(a.src.shared().1, "the foreign flush left the side biased");
    }
    // Read through the source: its driver reads ahead on the socket.
    let got = rt.block_on(async {
        let mut got = vec![0u8; filled.len() + data.len()];
        within(read_exact(&b.src, &mut got)).await.unwrap();
        got
    });
    rt.block_on(writer);
    assert!(
        got[..filled.len()] == filled[..],
        "the filled bytes were damaged"
    );
    assert!(
        got[filled.len()..] == data[..],
        "the write lost or reordered bytes"
    );
}

/// The shard moves to a spare thread while a read and a write wait on it:
/// the spare, which runs the same shard, serves them as the owner, and the
/// thread that was blocked, polling the sources after it is detached, is a
/// foreign one. Every byte arrives whole and in order.
#[test]
fn a_handoff_of_the_shard_during_a_read_and_a_write_loses_nothing() {
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(10)))
        .build()
        .unwrap();
    let (a, b) = pair();
    let (c, d) = pair();
    home_on_worker(&rt, &a, &b);
    home_on_worker(&rt, &c, &d);
    let read_data = pattern(512 * 1024);
    let write_data = pattern(2 << 20);
    let reader = {
        let src = Arc::clone(&b.src);
        rt.spawn(async move {
            let mut got = vec![0u8; 512 * 1024];
            within(read_exact(&src, &mut got)).await.unwrap();
            got
        })
    };
    let writer = {
        let (src, data) = (Arc::clone(&c.src), write_data.clone());
        rt.spawn(async move {
            within(write_all(&src, &data)).await.unwrap();
            within(flush(&src)).await.unwrap();
        })
    };
    // Waits out the handoff: the task blocks the worker's thread, then, with
    // the shard gone, polls the sources from that thread.
    let started = Arc::new(AtomicBool::new(false));
    let blocker = {
        let (started, src) = (Arc::clone(&started), Arc::clone(&a.src));
        rt.spawn(async move {
            started.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            let woke = Arc::new(Flag(AtomicBool::new(false)));
            let waker = Waker::from(Arc::clone(&woke));
            let mut cx = Context::from_waker(&waker);
            let _ = src.poll_flush(&mut cx);
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the blocking task never ran");
        std::thread::yield_now();
    }
    // Both transfers run while the first thread is blocked.
    let feeder = {
        let peer = a.io().try_clone().unwrap();
        let data = read_data.clone();
        std::thread::spawn(move || send(&peer, &data))
    };
    let sink = rt.block_on(async {
        let mut sink = vec![0u8; write_data.len()];
        within(read_exact(&d.src, &mut sink)).await.unwrap();
        sink
    });
    feeder.join().unwrap();
    assert!(rt.block_on(reader) == read_data, "the read lost bytes");
    rt.block_on(writer);
    rt.block_on(blocker);
    assert!(sink == write_data, "the write lost or reordered bytes");
}

const DATAGRAMS: u64 = 20_000;
const END: u64 = u64::MAX;

fn record(seq: u64) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&seq.to_le_bytes());
    bytes[8..].copy_from_slice(&seq.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes());
    bytes
}

/// Receives on `src` until an end record, checking each datagram whole and
/// seen once.
async fn receive_all(src: Arc<Source>, seen: Arc<Vec<AtomicBool>>) -> u64 {
    let mut count = 0;
    let mut buf = [0u8; 64];
    loop {
        let (n, _) = within(poll_fn(|cx| src.poll_receive(cx, &mut buf, false)))
            .await
            .unwrap();
        assert_eq!(n, 16, "a datagram came torn");
        let seq = u64::from_le_bytes(buf[..8].try_into().unwrap());
        if seq == END {
            return count;
        }
        assert_eq!(buf[..16], record(seq), "datagram {seq} was damaged");
        assert!(
            !seen[seq as usize].swap(true, Ordering::SeqCst),
            "datagram {seq} arrived twice"
        );
        count += 1;
    }
}

/// An owner on the worker and two foreign threads receive from one socket
/// at once, each datagram going to one of them: every one arrives whole,
/// once, and the side ends shared.
#[test]
fn an_owner_and_two_foreign_threads_polling_one_socket_lose_no_datagram() {
    let rt = runtime(None);
    let (a, b) = datagram_pair();
    let (a, b) = (Owned::new(a, DATAGRAM), Owned::new(b, DATAGRAM));
    {
        let src = Arc::clone(&b.src);
        rt.block_on(rt.spawn(async move {
            let mut buf = [0u8; 64];
            let _ = poll_fn(|cx| Poll::Ready(src.poll_receive(cx, &mut buf, false))).await;
        }));
    }
    let seen: Arc<Vec<AtomicBool>> =
        Arc::new((0..DATAGRAMS).map(|_| AtomicBool::new(false)).collect());
    let owner = {
        let (src, seen) = (Arc::clone(&b.src), Arc::clone(&seen));
        rt.spawn(receive_all(src, seen))
    };
    let foreign = || {
        let (src, seen, handle) = (Arc::clone(&b.src), Arc::clone(&seen), rt.handle());
        std::thread::spawn(move || handle.block_on(receive_all(src, seen)))
    };
    let (x, y) = (foreign(), foreign());
    let peer = a.io().try_clone().unwrap();
    let sender = std::thread::spawn(move || {
        let push = |bytes: [u8; 16]| loop {
            match peer.send(&bytes) {
                Ok(_) => return,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::yield_now(),
                Err(e) => panic!("{e}"),
            }
        };
        (0..DATAGRAMS).for_each(|seq| push(record(seq)));
        (0..3).for_each(|_| push(record(END)));
    });
    sender.join().unwrap();
    let got = rt.block_on(owner) + x.join().unwrap() + y.join().unwrap();
    assert_eq!(got, DATAGRAMS, "a datagram was lost");
    assert!(seen.iter().all(|s| s.load(Ordering::SeqCst)));
    if fenced() {
        assert!(b.src.shared().0, "foreign threads left the side biased");
    }
}

/// A thread whose heavy fence fails polls a side its worker owns: it leaves
/// its waker, the worker is asked to touch the side, the side goes shared and
/// the read completes with every byte, while a later source starts shared.
#[test]
fn a_foreign_read_without_a_fence_is_served_through_the_worker() {
    if !fenced() {
        return;
    }
    let rt = runtime(None);
    let (a, b) = pair();
    home_on_worker(&rt, &a, &b);
    let data = pattern(64 * 1024);
    let reader = {
        let (src, handle) = (Arc::clone(&b.src), rt.handle());
        std::thread::spawn(move || {
            crate::rt::handoff::fence::inject_failure(true);
            let got = handle.block_on(async {
                let mut got = vec![0u8; 64 * 1024];
                within(read_exact(&src, &mut got)).await.unwrap();
                got
            });
            let later = pair();
            let shared = later.0.src.shared();
            (got, shared)
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    send(a.io(), &data);
    let (got, later) = reader.join().unwrap();
    assert!(got == data, "the read lost or reordered bytes");
    assert_eq!(
        later,
        (true, true),
        "a source made after the failure is biased"
    );
}
