//! Requests that need both sides: the test holds a side itself, so the
//! request cannot run at once, and checks that nothing waits, that the work
//! is done by whoever lets go last, and that a descriptor closes only then.

use super::*;
use crate::io::Handle;
use crate::io::side::After;

/// Waits, the pause doubling up to 5 ms, until `done` gives a value; at the
/// deadline it fails with `what`.
#[cfg(unix)]
fn until_some<T>(what: &str, mut done: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(50);
    loop {
        if let Some(value) = done() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

/// The peer of a socket, polled once: still open (nothing to read), or at
/// its end.
fn peer_sees_end(peer: &Stream) -> bool {
    let mut buf = [0u8; 1];
    match (&*peer).read(&mut buf) {
        Ok(0) => true,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_handle_dropped_while_a_side_is_held_closes_once_the_holder_lets_go() {
    let (mine, peer) = stream_pair();
    peer.set_nonblocking(true).unwrap();
    let handle = Handle::new(mine).unwrap();
    let src = Arc::clone(handle.source());
    let held = src.read.try_hold().expect("the side is free");
    // The drop does not wait for the side: it hands its object over.
    drop(handle);
    assert!(
        !peer_sees_end(&peer),
        "the descriptor closed while a side was held"
    );
    let mut after = After::default();
    held.release(&mut after, &src.hooks());
    after.run();
    assert!(peer_sees_end(&peer), "the close never ran");
    assert!(src.is_retired());
}

/// Both sides held: the first to let go cannot take the other, so the close
/// waits for the second, which performs it.
#[test]
fn a_close_with_both_sides_held_is_done_by_the_last_to_let_go() {
    let (mine, peer) = stream_pair();
    peer.set_nonblocking(true).unwrap();
    let handle = Handle::new(mine).unwrap();
    let src = Arc::clone(handle.source());
    let read = src.read.try_hold().expect("the read side is free");
    let write = src.write.try_hold().expect("the write side is free");
    drop(handle);
    let mut after = After::default();
    read.release(&mut after, &src.hooks());
    assert!(
        !peer_sees_end(&peer),
        "closed with the write side still held"
    );
    write.release(&mut after, &src.hooks());
    after.run();
    assert!(peer_sees_end(&peer), "the close never ran");
}

/// `get_mut` must not hand the object out while a call of the driver may
/// use it: with a side held, it waits until the holder lets go and has
/// performed the retire.
#[test]
fn a_retire_waits_for_the_holder_that_performs_it() {
    let (mine, _peer) = stream_pair();
    let handle = Handle::new(mine).unwrap();
    let src = Arc::clone(handle.source());
    let held = src.write.try_hold().expect("the side is free");
    let (tell, retired) = std::sync::mpsc::channel();
    // A thread of its own, not scoped: if the retire never ends, the test
    // fails rather than waiting on that thread.
    std::thread::spawn(move || {
        let mut handle = handle;
        let _ = handle.get_mut();
        tell.send(handle).unwrap();
    });
    assert!(
        retired.recv_timeout(Duration::from_millis(50)).is_err(),
        "get_mut returned while a side was held"
    );
    let mut after = After::default();
    held.release(&mut after, &src.hooks());
    after.run();
    let handle = retired
        .recv_timeout(Duration::from_secs(10))
        .expect("the retire never finished");
    assert!(src.is_retired(), "the source was not retired");
    drop(handle);
}

/// A handle dropped with bytes behind while a side is held: the hand-off to
/// the linger, on the handle's own object, is done by the holder, and the
/// bytes still reach the peer before it sees the end.
#[test]
fn a_dropped_handle_lingers_through_a_held_side() {
    let rt = runtime(None);
    let (mine, peer) = stream_pair();
    let (sent, src) = rt.block_on(async {
        let handle = Handle::new(mine).unwrap();
        let mut sent = Vec::new();
        let chunk = pattern(64 * 1024);
        let start = Instant::now();
        while !handle.source().has_unsent() {
            let mut rest = &chunk[..];
            while !rest.is_empty() {
                let n = within(poll_fn(|cx| handle.source().poll_write(cx, rest)))
                    .await
                    .unwrap();
                sent.extend_from_slice(&rest[..n]);
                rest = &rest[n..];
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "nothing went behind"
            );
        }
        let src = Arc::clone(handle.source());
        let held = src.read.try_hold().expect("the read side is free");
        drop(handle);
        let mut after = After::default();
        held.release(&mut after, &src.hooks());
        after.run();
        (sent, src)
    });
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut got = Vec::new();
    (&peer).read_to_end(&mut got).unwrap();
    assert!(got == sent, "{} of {} bytes arrived", got.len(), sent.len());
    drop(rt);
    assert!(src.is_retired(), "the lingering object was never closed");
}

/// The drain of a lingering close may wait on the write side before the
/// close runs: the close wakes it, and it goes on through the handle's own
/// descriptor, whose object the drain keeps.
#[cfg(unix)]
#[test]
fn a_drain_waiting_before_a_lingering_close_is_woken_by_it() {
    let rt = runtime(None);
    let (mine, peer) = stream_pair();
    adopt(&mine);
    let src = Source::new(mine.raw(), STREAM);
    let woken = Arc::new(Flag(AtomicBool::new(false)));
    // How far the test got, for the watchdog's report.
    let phase = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let _dog = {
        // Weak: the watchdog must not change when the source goes.
        let (phase, src) = (Arc::clone(&phase), Arc::downgrade(&src));
        Watchdog::new(
            "a_drain_waiting_before_a_lingering_close_is_woken_by_it",
            Duration::from_secs(30),
            move || {
                let sides = src.upgrade().map_or_else(
                    || "the source is gone".to_string(),
                    |src| {
                        format!(
                            "read side {}; write side {}",
                            src.read_report(),
                            src.write_report()
                        )
                    },
                );
                format!(
                    "phase {}; {sides}; {:?}",
                    phase.load(Ordering::SeqCst),
                    crate::stats()
                )
            },
        )
    };
    // The kernel takes nothing more: what the source accepts next waits.
    fill(&mine);
    rt.block_on(async {
        let chunk = pattern(64 * 1024);
        let start = Instant::now();
        while !src.has_unsent() {
            within(poll_fn(|cx| src.poll_write(cx, &chunk)))
                .await
                .unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "nothing went behind"
            );
        }
    });
    phase.store(1, Ordering::SeqCst);
    // The runtime took the bytes behind over as `block_on` returned: its
    // drain task may hold a side for a moment, and then performs the close
    // as it lets go.
    let held = until_some("the read side never came free", || src.read.try_hold());
    src.linger(mine.raw());
    let waker = Waker::from(Arc::clone(&woken));
    assert!(
        src.poll_drain(&mut Context::from_waker(&waker))
            .is_pending(),
        "the peer reads nothing, so the drain waits"
    );
    phase.store(2, Ordering::SeqCst);
    let mut after = After::default();
    held.release(&mut after, &src.hooks());
    after.run();
    until_some("the drain waiting before the close was never woken", || {
        woken.0.load(Ordering::SeqCst).then_some(())
    });
    phase.store(3, Ordering::SeqCst);
    drop(peer);
    src.close(mine);
    phase.store(4, Ordering::SeqCst);
    drop(rt);
}

/// An attach performed off the ring's thread hands the ring a lease on the
/// handle's object, whose descriptor it puts into its table. The handle
/// closes before the ring gets to it: the lease keeps the object open until
/// then, and the ring must not keep the file after, or the peer never sees
/// the end. The ring's thread is held in a task meanwhile, so the order is
/// certain.
#[cfg(ring)]
#[test]
fn a_close_before_the_ring_installs_an_attach_leaves_it_no_file() {
    use std::sync::mpsc::channel;
    let rt = runtime(None);
    let on_ring = rt.block_on(
        rt.spawn(async { crate::proactor::Proactor::backend() == crate::IoBackend::IoUring }),
    );
    if !on_ring {
        eprintln!("skipping: the runtime's shard runs no ring here");
        return;
    }
    let (mine, peer) = stream_pair();
    let handle = Handle::new(mine).unwrap();
    let src = Arc::clone(handle.source());
    // The write side held here: the read the worker polls registers the
    // source, and the attach is left to this thread.
    let held = src.write.try_hold().expect("the write side is free");
    let (polled, has_polled) = channel();
    let (go, gone) = channel::<()>();
    let reader = Arc::clone(&src);
    let task = rt.spawn(async move {
        let mut buf = [0u8; 8];
        let mut read = Box::pin(poll_fn(|cx| reader.poll_read(cx, &mut buf, false)));
        pends(&mut read).await;
        drop(read);
        polled.send(()).unwrap();
        // The ring's thread waits here, its posted work not applied yet.
        gone.recv().unwrap();
    });
    has_polled
        .recv_timeout(Duration::from_secs(10))
        .expect("the worker's read never ran");
    // Letting go performs the attach here, which posts a lease to the ring.
    let mut after = After::default();
    held.release(&mut after, &src.hooks());
    after.run();
    drop(handle);
    go.send(()).unwrap();
    rt.block_on(task);
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(
        (&peer).read(&mut buf).unwrap(),
        0,
        "the peer never saw the end: the ring kept the file"
    );
}

/// An attach to a ring off the ring's thread is undone (its shard evicted
/// the source) before the ring gets to it, and the source attaches to
/// another ring meanwhile, under a token of the same number (every shard
/// numbers its own). The first ring must not take that attach for its own:
/// it would put the file into its table and hand the source a slot of it,
/// which the source would then name on the other ring. Both rings' threads
/// are held in a task meanwhile, so the order is certain.
#[cfg(ring)]
#[test]
fn a_stale_attach_is_not_taken_for_one_to_another_ring_under_the_same_token() {
    use std::sync::mpsc::channel;
    let (a, b) = (runtime(None), runtime(None));
    let on_ring = a.block_on(
        a.spawn(async { crate::proactor::Proactor::backend() == crate::IoBackend::IoUring }),
    );
    if !on_ring {
        eprintln!("skipping: the runtime's shard runs no ring here");
        return;
    }
    let (mine, peer) = stream_pair();
    let handle = Handle::new(mine).unwrap();
    let src = Arc::clone(handle.source());
    // A read polled on `rt`'s worker registers the source there, the attach
    // left to this thread (it holds the write side), which then performs it
    // and posts a lease to the ring; the worker's thread waits for `go`.
    let attach_held = |rt: &Runtime| {
        let held = src.write.try_hold().expect("the write side is free");
        let (polled, has_polled) = channel();
        let (go, gone) = channel::<()>();
        let reader = Arc::clone(&src);
        let task = rt.spawn(async move {
            let mut buf = [0u8; 8];
            let mut read = Box::pin(poll_fn(|cx| reader.poll_read(cx, &mut buf, false)));
            pends(&mut read).await;
            drop(read);
            polled.send(()).unwrap();
            gone.recv().unwrap();
        });
        has_polled
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker's read never ran");
        let mut after = After::default();
        held.release(&mut after, &src.hooks());
        after.run();
        (go, task)
    };
    let (go_a, task_a) = attach_held(&a);
    let (home_a, token) = src.registration();
    // The first shard evicts the source: its attach under way is undone.
    src.evict();
    let (go_b, task_b) = attach_held(&b);
    let (home_b, token_b) = src.registration();
    assert_ne!(home_a, home_b);
    assert_eq!(token, token_b, "the two shards' first tokens differ");
    // The first ring gets to its stale attach: its worker applies what was
    // posted to it before it sleeps.
    go_a.send(()).unwrap();
    a.block_on(task_a);
    let in_a = a.block_on(a.spawn(async {
        crate::time::sleep(Duration::from_millis(10)).await;
        crate::rt::context::current(|s| s.and_then(|s| s.ring().map(|r| r.files_in_use().0)))
    }));
    assert_eq!(in_a, Some(0), "the first ring put the file into its table");
    // The second ring installs its own, and a read through it gets the
    // peer's bytes.
    go_b.send(()).unwrap();
    b.block_on(task_b);
    (&peer).write_all(b"x").unwrap();
    let reader = Arc::clone(&src);
    let got = b.block_on(b.spawn(async move {
        let mut buf = [0u8; 8];
        within(poll_fn(|cx| reader.poll_read(cx, &mut buf, false)))
            .await
            .map(|n| buf[..n].to_vec())
    }));
    assert_eq!(got.unwrap(), b"x");
    drop(handle);
}

/// A shard that stops running while another thread holds a side of a
/// source registered to it: the holder evicts the source as it lets go, a
/// driver call later, and may hand the shard's driver an operation until
/// then; the shard waits for that eviction before it looks at what its
/// driver has out, so it never stops with the source still registered to it.
#[test]
fn a_shard_that_leaves_waits_for_the_eviction_a_holder_performs() {
    let (mine, _peer) = stream_pair();
    let handle = Handle::new(mine).unwrap();
    let src = Arc::clone(handle.source());
    let (token_tx, token_rx) = std::sync::mpsc::channel::<(usize, u64)>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let left = Arc::new(AtomicBool::new(false));
    let shard = {
        let (src, left) = (Arc::clone(&src), Arc::clone(&left));
        std::thread::spawn(move || {
            let holder = crate::block_on(async {
                // One read polled here registers the source with this
                // thread's own shard.
                let mut buf = [0u8; 1];
                {
                    let mut read = Box::pin(handle.recv(&mut buf));
                    let polled = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
                    assert!(polled.is_pending());
                }
                token_tx.send(src.registration()).unwrap();
                let (held_tx, held_rx) = std::sync::mpsc::channel::<()>();
                let holder = {
                    let src = Arc::clone(&src);
                    std::thread::spawn(move || {
                        let held = src.read.try_hold().expect("the side is free");
                        held_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        let mut after = After::default();
                        held.release(&mut after, &src.hooks());
                        after.run();
                    })
                };
                held_rx.recv().unwrap();
                holder
            });
            left.store(true, Ordering::SeqCst);
            (holder, handle)
        })
    };
    let (home, token) = token_rx.recv().unwrap();
    // Past the ring's wait for its operations as a shard goes (one second),
    // which a shard that did not wait for the eviction would end by.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !left.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::yield_now();
    }
    if left.load(Ordering::SeqCst) {
        assert!(
            !src.registered_with(home, token),
            "the shard stopped with the source still registered to it"
        );
    }
    release_tx.send(()).unwrap();
    let (holder, handle) = shard.join().unwrap();
    holder.join().unwrap();
    assert!(!src.registered_with(home, token), "the eviction never ran");
    drop(handle);
}
