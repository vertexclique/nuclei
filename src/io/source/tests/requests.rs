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
        let chunk = pattern(BEHIND_CHUNK);
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
        let chunk = pattern(BEHIND_CHUNK);
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

/// A shard that leaves while another thread holds a side of a source registered to it: the holder
/// evicts the source as it lets go, and the shard applies that eviction's detach before it leaves,
/// which takes the source's file out of the ring and lets go of the lease that keeps the handle's
/// object open. Before, the registration read as gone before its detach reached the shard's
/// mailbox: the shard's wait could end in between and the shard leave, and a thread's own shard
/// runs again only at its next `block_on`, so the detach waited there with its lease, and the
/// handle, dropped, never closed (a dropped stream's linger delivered every byte and its peer
/// never saw the end: `drops::a_runtime_dropped_with_bytes_behind_delivers_them`). The holder's
/// post waits here, should the registration read as gone by then, until the shard left, which
/// makes that order certain.
#[cfg(ring)]
#[test]
fn a_shard_that_leaves_applies_the_detach_of_a_holders_eviction() {
    use std::sync::mpsc::channel;
    let (mine, peer) = stream_pair();
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let handle = Handle::new(mine).unwrap();
    let src = Arc::clone(handle.source());
    let (registered, has_registered) = channel::<Option<(usize, u64)>>();
    let (release, released) = channel::<()>();
    let (give, taken) = channel();
    let (exit, exits) = channel::<()>();
    let left = Arc::new(AtomicBool::new(false));
    let ended_first = Arc::new(AtomicBool::new(false));
    let shard = {
        let (src, left, ended_first) = (
            Arc::clone(&src),
            Arc::clone(&left),
            Arc::clone(&ended_first),
        );
        std::thread::spawn(move || {
            let holder = crate::block_on(async {
                if crate::proactor::Proactor::backend() != crate::IoBackend::IoUring {
                    registered.send(None).unwrap();
                    return None;
                }
                // One read polled here registers the source with this thread's own shard.
                let mut buf = [0u8; 1];
                {
                    let mut read = Box::pin(handle.recv(&mut buf));
                    let polled = poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
                    assert!(polled.is_pending());
                }
                let (home, token) = src.registration();
                registered.send(Some((home, token))).unwrap();
                let (held_tx, held_rx) = channel::<()>();
                let holder = {
                    let (src, left) = (Arc::clone(&src), Arc::clone(&left));
                    std::thread::spawn(move || {
                        let held = src.read.try_hold().expect("the side is free");
                        held_tx.send(()).unwrap();
                        released.recv().unwrap();
                        let looked = Arc::clone(&src);
                        super::super::ring::BEFORE_DETACH_POST.with(|look| {
                            *look.borrow_mut() = Some(Box::new(move || {
                                if !looked.registered_with(home, token) {
                                    ended_first.store(true, Ordering::SeqCst);
                                    until_some("the shard never left", || {
                                        left.load(Ordering::SeqCst).then_some(())
                                    });
                                }
                            }));
                        });
                        let mut after = After::default();
                        held.release(&mut after, &src.hooks());
                        after.run();
                        super::super::ring::BEFORE_DETACH_POST
                            .with(|look| *look.borrow_mut() = None);
                    })
                };
                held_rx.recv().unwrap();
                Some(holder)
            });
            left.store(true, Ordering::SeqCst);
            if let Some(holder) = holder {
                holder.join().unwrap();
            }
            give.send(handle).unwrap();
            // The shard stays as it left, not dropped, until the test looked.
            exits.recv().unwrap();
        })
    };
    let Some((home, token)) = has_registered.recv().unwrap() else {
        eprintln!("skipping: this thread's shard runs no ring here");
        drop(release);
        drop(taken.recv().unwrap());
        exit.send(()).unwrap();
        shard.join().unwrap();
        return;
    };
    assert_ne!(home, 0, "the read did not register the source");
    // The shard's leave asked for the eviction, which waits for the holder.
    until_some("the shard never asked for the eviction", || {
        (src.requests.load(Ordering::SeqCst) != 0).then_some(())
    });
    release.send(()).unwrap();
    let handle = taken
        .recv_timeout(Duration::from_secs(20))
        .expect("the shard never left");
    assert!(!src.registered_with(home, token), "the eviction never ran");
    drop(handle);
    let mut buf = [0u8; 1];
    let end = (&peer).read(&mut buf);
    assert!(
        matches!(end, Ok(0)),
        "the peer never saw the end ({end:?}): the shard left with the eviction's detach still to \
         come, its ring holding the file and its mailbox the lease (the registration read as gone \
         before the detach was posted: {})",
        ended_first.load(Ordering::SeqCst)
    );
    assert!(
        !ended_first.load(Ordering::SeqCst),
        "the registration read as gone before its detach reached the shard"
    );
    exit.send(()).unwrap();
    shard.join().unwrap();
}

/// A thread's own shard leaves while the attach of a source it registered is still a request,
/// left to this thread, which holds a side: letting go performs the attach and the eviction, which
/// post the file's install (with a lease that keeps the handle's object open) and the slot's
/// release to the shard. The shard waits for both before it leaves and applies them, so no lease is
/// left with it while it is away, and the release never reaches the registration its next
/// `block_on` makes under the same slot. Before, the shard left at once: the lease kept the
/// descriptor open until its next `block_on`, whose first registration the stale release then took
/// out of its table, so that source was never evicted (`proofs/tla/going/Pending.tla`).
#[cfg(ring)]
#[test]
fn a_shard_that_leaves_waits_for_an_attach_left_to_a_holder() {
    use std::sync::mpsc::channel;
    let (mine, _peer) = stream_pair();
    let handle = Handle::new(mine).unwrap();
    let src = Arc::clone(handle.source());
    let (second, _second_peer) = stream_pair();
    let second = Handle::new(second).unwrap();
    let src2 = Arc::clone(second.source());
    let held = src.write.try_hold().expect("the write side is free");
    let (polled, has_polled) = channel::<bool>();
    let (again, go_again) = channel::<()>();
    let (ended, has_ended) = channel();
    let left = Arc::new(AtomicBool::new(false));
    let waits = super::super::ATTACH_WAITS.load(Ordering::SeqCst);
    let shard = {
        let left = Arc::clone(&left);
        std::thread::spawn(move || {
            crate::block_on(async {
                let on_ring = crate::proactor::Proactor::backend() == crate::IoBackend::IoUring;
                if on_ring {
                    // The read registers the source here; its attach is left to the holder.
                    let mut buf = [0u8; 1];
                    let mut read = Box::pin(handle.recv(&mut buf));
                    assert!(
                        poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx)))
                            .await
                            .is_pending()
                    );
                }
                polled.send(on_ring).unwrap();
            });
            left.store(true, Ordering::SeqCst);
            if go_again.recv().is_ok() {
                crate::block_on(async {
                    // The next block_on's first registration takes the slot again.
                    let mut buf = [0u8; 1];
                    let mut read = Box::pin(second.recv(&mut buf));
                    assert!(
                        poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx)))
                            .await
                            .is_pending()
                    );
                    drop(read);
                    // A turn with nothing to run applies what the mailbox holds.
                    crate::time::sleep(Duration::from_millis(1)).await;
                });
            }
            ended.send((handle, second)).unwrap();
        })
    };
    let on_ring = has_polled.recv().unwrap();
    let mut after = After::default();
    if !on_ring {
        eprintln!("skipping: this thread's shard runs no ring here");
        held.release(&mut after, &src.hooks());
        after.run();
        drop(again);
        drop(has_ended.recv().unwrap());
        shard.join().unwrap();
        return;
    }
    // The shard left, or waits in its leave for the attach still asked of it.
    until_some("the shard neither left nor waited", || {
        (left.load(Ordering::SeqCst) || super::super::ATTACH_WAITS.load(Ordering::SeqCst) > waits)
            .then_some(())
    });
    held.release(&mut after, &src.hooks());
    after.run();
    until_some("the shard never left", || {
        left.load(Ordering::SeqCst).then_some(())
    });
    assert!(
        !src.is_leased(),
        "the shard left with the install's lease still in its mailbox"
    );
    again.send(()).unwrap();
    let (handle, second) = has_ended
        .recv_timeout(Duration::from_secs(20))
        .expect("the shard's next block_on never ended");
    assert_eq!(
        src2.registration().0,
        0,
        "a stale release took the next registration out of the shard's table: it was never evicted"
    );
    drop((handle, second));
    shard.join().unwrap();
}

/// A registration posted to a shard that stops before it makes it fails no IO of the source: the
/// source is unregistered again (`proofs/tla/attach`: an attach refused leaves it so), and its
/// next write and read go through where they are polled. Before, the source kept the failure
/// (a dropped stream's drain took it as the drain's end and gave its bytes up) and stayed
/// registering, so no later wait registered it again.
#[test]
fn a_registration_a_stopped_shard_never_made_fails_no_io() {
    use crate::rt::shard::{IoOp, Limits, Shard};
    let (a, b) = pair();
    // A wait on a thread that runs no shard posted the registration.
    a.src
        .reg
        .state
        .store(super::super::REGISTERING, Ordering::Release);
    let stopped = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    })
    .unwrap();
    stopped.shared.close_io();
    stopped.shared.post_io(IoOp::Register(Arc::clone(&a.src)));
    assert_eq!(
        a.src.reg.state.load(Ordering::Acquire),
        super::super::UNREGISTERED,
        "the source still waits for a registration nobody makes"
    );
    let rt = runtime(None);
    rt.block_on(async {
        write_all(&a.src, b"after").await.unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(within(read(&b.src, &mut buf)).await.unwrap(), 5);
        assert_eq!(&buf[..5], b"after");
    });
}

/// The data loss behind the first: a drain of bytes behind, whose source's registration was posted
/// to a shard that stopped before it made it, delivers every byte. Before, the drain took the
/// refusal for its end with the bytes still behind, and a stream dropped with its runtime gave
/// them up (a cut stream its peer read as whole, later a reset). Unix only, as the drain test
/// above.
#[cfg(unix)]
#[test]
fn a_drain_whose_registration_a_stopped_shard_never_made_delivers_every_byte() {
    use crate::rt::shard::{IoOp, Limits, Shard};
    let rt = runtime(None);
    let (mine, peer) = stream_pair();
    adopt(&mine);
    let src = Source::new(mine.raw(), STREAM);
    // The kernel takes nothing more: what the source accepts next waits behind.
    let mut expected = fill(&mine);
    let chunk = pattern(BEHIND_CHUNK);
    rt.block_on(async {
        let start = Instant::now();
        while !src.has_unsent() {
            let n = within(poll_fn(|cx| src.poll_write(cx, &chunk)))
                .await
                .unwrap();
            expected.extend_from_slice(&chunk[..n]);
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "nothing went behind"
            );
        }
    });
    // Unregistered, then a wait on a thread that runs no shard posted the registration again,
    // to a shard that stops before it makes it.
    src.evict();
    src.reg
        .state
        .store(super::super::REGISTERING, Ordering::Release);
    let stopped = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    })
    .unwrap();
    stopped.shared.close_io();
    stopped.shared.post_io(IoOp::Register(Arc::clone(&src)));
    let reader = {
        let mut peer = peer.try_clone().unwrap();
        let want = expected.len();
        std::thread::spawn(move || {
            peer.set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut got = vec![0u8; want];
            peer.read_exact(&mut got).map(|()| got)
        })
    };
    rt.block_on(within(poll_fn(|cx| src.poll_drain(cx))));
    assert!(!src.has_unsent(), "the drain ended with bytes still behind");
    let got = reader.join().unwrap().expect("the peer got every byte");
    assert!(
        got == expected,
        "the peer got other bytes than were written"
    );
    drop(peer);
    src.close(mine);
    drop(rt);
}

/// An object wider than a word, which a dropped handle hands over boxed: it is dropped once, by
/// the last to let go, as one that fits in the word (a socket) is.
#[cfg(unix)]
struct Wide {
    stream: Stream,
    drops: Arc<std::sync::atomic::AtomicUsize>,
    _room: [u64; 3],
}

#[cfg(unix)]
impl std::os::fd::AsFd for Wide {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

#[cfg(unix)]
impl Drop for Wide {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(unix)]
#[test]
fn a_dropped_handle_of_an_object_wider_than_a_word_drops_it_once_the_holder_lets_go() {
    let (mine, peer) = stream_pair();
    peer.set_nonblocking(true).unwrap();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = Handle::new(Wide {
        stream: mine,
        drops: Arc::clone(&drops),
        _room: [0; 3],
    })
    .unwrap();
    let src = Arc::clone(handle.source());
    let held = src.read.try_hold().expect("the side is free");
    drop(handle);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        0,
        "dropped while a side was held"
    );
    assert!(
        !peer_sees_end(&peer),
        "the descriptor closed while a side was held"
    );
    let mut after = After::default();
    held.release(&mut after, &src.hooks());
    after.run();
    assert_eq!(drops.load(Ordering::SeqCst), 1, "not dropped exactly once");
    assert!(peer_sees_end(&peer), "the close never ran");
    drop(src);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "dropped again with the source"
    );
}

/// A runtime dropped while a task holds a stream that waits for bytes: the worker's shard,
/// going, evicts the source and drops the task, which closes the stream, both as the sides'
/// owner (no foreign take switched them), and the peer sees the end.
#[cfg(unix)]
#[test]
fn a_runtime_dropped_under_a_waiting_stream_closes_it_as_its_owner() {
    if !crate::rt::handoff::fence::available() {
        eprintln!("skipping: no heavy fence here, so every side starts shared");
        return;
    }
    let (mine, peer) = stream_pair();
    peer.set_nonblocking(true).unwrap();
    let rt = runtime(None);
    // A ring's completions that come as it settles are its own thread's to take, not a request.
    let on_ring = rt.block_on(
        rt.spawn(async { crate::proactor::Proactor::backend() == crate::IoBackend::IoUring }),
    );
    let (polled_tx, polled_rx) = std::sync::mpsc::channel();
    let task = rt.spawn(async move {
        let handle = Handle::new(mine).unwrap();
        polled_tx.send(Arc::clone(handle.source())).unwrap();
        let mut buf = [0u8; 1];
        let _ = handle.recv(&mut buf).await;
        drop(handle);
    });
    let src = polled_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the task never ran");
    until_some("the read never registered the source", || {
        (src.registration().0 != 0).then_some(())
    });
    task.detach();
    drop(rt);
    assert!(
        peer_sees_end(&peer),
        "the dropped task's stream never closed"
    );
    if on_ring {
        eprintln!("skipping the look at the sides: the shard runs a ring here");
        return;
    }
    assert_eq!(
        src.shared(),
        (false, false),
        "the teardown took a side as a foreign thread"
    );
}

/// The source's request word once it differs from `before`: a request was raised meanwhile.
fn until_requested(src: &Source, before: u8, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(50);
    while src.requests.load(Ordering::Acquire) == before {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

/// The first read after a retire, on the same descriptor, reads the peer's bytes.
fn reads_after_the_retire(rt: &Runtime, a: &Owned<Stream>, b: &Owned<Stream>) {
    a.src.revive(a.io().raw());
    send(b.io(), b"after");
    rt.block_on(async {
        let mut buf = [0u8; 16];
        let n = within(read(&a.src, &mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"after");
    });
}

/// An attach left to a holder of the read side, then a retire (`get_mut`) left to the same holder:
/// the holder performs both at once, the retire first, so the attach finds the descriptor out of
/// the source. That refusal fails nothing, as one for a shard that stopped: the source is
/// unregistered, and the first read after the retire registers it again and reads. Before, the
/// refusal failed both sides as a failed registration does, and that read answered NotConnected
/// on a valid handle (`proofs/tla/attach/Merged.tla`, `M_holder_fails`).
#[test]
fn an_attach_performed_with_a_retire_fails_no_read_after_it() {
    let rt = runtime(None);
    let (a, b) = pair();
    let held = a.src.read.try_hold().expect("the side is free");
    let src = Arc::clone(&a.src);
    rt.block_on(rt.spawn(async move { src.register() }))
        .unwrap();
    assert_eq!(
        a.src.reg.state.load(Ordering::Acquire),
        super::super::REGISTERING,
        "the attach did not wait for the holder"
    );
    let attach = a.src.requests.load(Ordering::Acquire);
    let src = Arc::clone(&a.src);
    let retirer = std::thread::spawn(move || src.retire());
    until_requested(&a.src, attach, "the retire was never asked");
    let mut after = After::default();
    held.release(&mut after, &a.src.hooks());
    after.run();
    retirer.join().unwrap();
    assert_eq!(
        a.src.reg.state.load(Ordering::Acquire),
        super::super::UNREGISTERED,
        "the refused attach left the source registering"
    );
    reads_after_the_retire(&rt, &a, &b);
}

/// A registration posted to a shard by a thread that runs none, which the shard makes only after
/// a retire: the shard performs the attach itself and hears it refused for the missing
/// descriptor, which is no registration failure: the first read after the retire registers the
/// source again and reads. Before, the shard left the refusal for the sides as a failure, and
/// that read answered it (`proofs/tla/attach/Merged.tla`, `M_requester_fails`).
#[test]
fn a_registration_made_after_a_retire_fails_no_read_after_it() {
    use crate::rt::shard::{IoOp, Limits, Shard};
    let (a, b) = pair();
    a.src
        .reg
        .state
        .store(super::super::REGISTERING, Ordering::Release);
    let shard = Shard::new(Limits {
        max_nesting: 8,
        buffer_cap: crate::buf::DEFAULT_CAP,
        backend: crate::rt::backend::chosen().unwrap(),
    })
    .unwrap();
    shard.shared.post_io(IoOp::Register(Arc::clone(&a.src)));
    a.src.retire();
    shard.block_on(crate::task::yield_now());
    assert_eq!(
        a.src.reg.state.load(Ordering::Acquire),
        super::super::UNREGISTERED,
        "the shard never made the registration"
    );
    drop(shard);
    reads_after_the_retire(&runtime(None), &a, &b);
}

/// A connected TCP pair over the loopback.
#[cfg(unix)]
fn tcp_pair() -> (std::net::TcpStream, std::net::TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (b, _) = listener.accept().unwrap();
    (a, b)
}

#[cfg(unix)]
#[test]
fn unacked_counts_what_the_peer_has_not_taken() {
    let (mut writer, mut peer) = tcp_pair();
    let fd = writer.as_raw_fd();
    writer.write_all(&[7u8; 1000]).unwrap();
    let mut got = [0u8; 1000];
    peer.read_exact(&mut got).unwrap();
    if cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd"
    )) {
        assert!(sys::unacked(fd).is_some(), "the system has a count");
    }
    until_some(
        "the peer never acknowledged the bytes",
        || match sys::unacked(fd) {
            Some(0) | None => Some(()),
            Some(_) => None,
        },
    );
}

#[cfg(unix)]
#[test]
fn the_wait_for_acks_ends_once_the_peer_acknowledged() {
    use crate::io::handle::wait_for_acks;
    let rt = runtime(None);
    let (mut writer, mut peer) = tcp_pair();
    let fd = writer.as_raw_fd();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0u8; 100_000];
        peer.read_exact(&mut buf).unwrap();
    });
    writer.write_all(&vec![1u8; 100_000]).unwrap();
    let start = Instant::now();
    rt.block_on(wait_for_acks(fd, Duration::from_secs(10)));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the wait outlasted the acknowledgements"
    );
    reader.join().unwrap();
    if cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd"
    )) {
        assert_eq!(sys::unacked(fd), Some(0), "every byte acknowledged");
    }
}

/// A loopback pair whose peer's window is closed from the handshake on, the writer's kernel left
/// holding bytes the peer has not taken.
#[cfg(unix)]
fn tcp_pair_with_a_closed_window() -> (std::net::TcpStream, std::net::TcpStream, usize) {
    let set_buf = |fd: libc::c_int, opt: libc::c_int, size: libc::c_int| {
        // SAFETY: `fd` is open for the call and `size` is a `c_int` of the length given.
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&raw const size).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(ret, 0, "{}", io::Error::last_os_error());
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    set_buf(listener.as_raw_fd(), libc::SO_RCVBUF, 4096);
    let mut writer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    set_buf(writer.as_raw_fd(), libc::SO_SNDBUF, 256 * 1024);
    writer.set_nonblocking(true).unwrap();
    let mut written = 0;
    loop {
        match writer.write(&[0u8; 4096]) {
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("{e}"),
        }
    }
    (writer, peer, written)
}

#[cfg(unix)]
#[test]
fn the_wait_for_acks_ends_at_its_deadline() {
    use crate::io::handle::wait_for_acks;
    let rt = runtime(None);
    let (writer, peer, _) = tcp_pair_with_a_closed_window();
    let fd = writer.as_raw_fd();
    let counts = cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd"
    ));
    assert_eq!(sys::unacked(fd).is_some(), counts, "the system's count");
    if !counts {
        return;
    }
    assert!(
        sys::unacked(fd).is_some_and(|n| n > 0),
        "bytes the peer has not taken"
    );
    let start = Instant::now();
    rt.block_on(wait_for_acks(fd, Duration::from_millis(200)));
    let took = start.elapsed();
    assert!(
        took >= Duration::from_millis(200),
        "returned early: {took:?}"
    );
    assert!(took < Duration::from_secs(5), "returned late: {took:?}");
    assert!(
        sys::unacked(fd).is_some_and(|n| n > 0),
        "the deadline, not the acknowledgements, ended the wait"
    );
    drop(peer);
}

#[cfg(unix)]
const COUNTS_UNACKED: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
));

#[cfg(unix)]
#[test]
fn a_zero_linger_mark_follows_the_linger_set() {
    let rt = runtime(None);
    rt.block_on(async {
        let (a, _b) = tcp_pair();
        let handle = Handle::new(a).unwrap();
        assert!(!handle.source().zero_linger());
        handle.set_zero_linger().unwrap();
        assert!(handle.source().zero_linger());
        handle.set_linger(None).unwrap();
        assert!(!handle.source().zero_linger());
        handle.set_linger(Some(Duration::from_millis(500))).unwrap();
        assert!(handle.source().zero_linger());
        handle.set_linger(Some(Duration::from_secs(1))).unwrap();
        assert!(!handle.source().zero_linger());
    });
}

#[cfg(unix)]
#[test]
fn a_socket_set_to_zero_linger_passes_it_to_its_streams() {
    use crate::net::TcpSocket;
    use futures_core::Stream;
    let rt = runtime(None);
    rt.block_on(async {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_zero_linger().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = socket.local_addr().unwrap();
        let listener = socket.listen(16).unwrap();
        assert!(listener.source().zero_linger());

        let marked = TcpSocket::new_v4().unwrap();
        marked.set_zero_linger().unwrap();
        let client = marked.connect(addr).await.unwrap();
        assert!(client.source().zero_linger());
        let (accepted, _) = listener.accept().await.unwrap();
        assert!(accepted.source().zero_linger());

        let plain = TcpSocket::new_v4().unwrap().connect(addr).await.unwrap();
        assert!(!plain.source().zero_linger());
        let (accepted, _) = listener.accept().await.unwrap();
        assert!(accepted.source().zero_linger());

        let mut multi = core::pin::pin!(listener.accept_multi().await.unwrap());
        let _third = TcpSocket::new_v4().unwrap().connect(addr).await.unwrap();
        let taken = core::future::poll_fn(|cx| multi.as_mut().poll_next(cx))
            .await
            .unwrap();
        assert!(taken.source().zero_linger());
    });
}

#[cfg(unix)]
#[test]
fn waits_for_acks_asks_the_kernel_only_for_a_marked_stream() {
    let rt = runtime(None);
    let (writer, peer, _) = tcp_pair_with_a_closed_window();
    let fd = writer.as_raw_fd();
    rt.block_on(async {
        let handle = Handle::new(writer).unwrap();
        assert!(!handle.source().waits_for_acks(fd));
        handle.set_zero_linger().unwrap();
        assert_eq!(handle.source().waits_for_acks(fd), COUNTS_UNACKED);
        handle.set_linger(None).unwrap();
        assert!(!handle.source().waits_for_acks(fd));
    });
    drop(peer);
}

#[cfg(unix)]
#[test]
fn a_zero_linger_stream_dropped_with_nothing_behind_delivers_what_its_kernel_holds_then_resets() {
    let rt = runtime(None);
    let (writer, mut peer, written) = tcp_pair_with_a_closed_window();
    if !COUNTS_UNACKED {
        assert!(sys::unacked(writer.as_raw_fd()).is_none());
        return;
    }
    rt.block_on(async {
        let handle = Handle::new(writer).unwrap();
        handle.set_zero_linger().unwrap();
        drop(handle);
    });
    peer.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let mut sum = 0;
    loop {
        match peer.read(&mut buf) {
            Ok(0) => panic!("an end, not a reset"),
            Ok(n) => sum += n,
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(sum, written, "every byte the kernel held, then the reset");
    drop(rt);
}
