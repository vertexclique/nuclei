//! Requests that need both sides: the test holds a side itself, so the
//! request cannot run at once, and checks that nothing waits, that the work
//! is done by whoever lets go last, and that a descriptor closes only then.

use super::*;
use crate::io::Handle;
use crate::io::side::After;

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
/// the copy of the descriptor is done by the holder, and the bytes still
/// reach the peer before it sees the end.
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
    assert!(src.is_retired(), "the copy was never closed");
}

/// The drain of a lingering close may wait on the write side before the
/// close runs: the close wakes it, and it goes on through the copy (a
/// descriptor's; on Windows the handle keeps its object instead, see
/// `a_dropped_handle_lingers_through_a_held_side`).
#[cfg(unix)]
#[test]
fn a_drain_waiting_before_a_lingering_close_is_woken_by_it() {
    let rt = runtime(None);
    let (mine, peer) = stream_pair();
    adopt(&mine);
    let src = Source::new(mine.raw(), STREAM);
    let woken = Arc::new(Flag(AtomicBool::new(false)));
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
    let held = src.read.try_hold().expect("the read side is free");
    let copy = src.duplicate().expect("a copy of the descriptor");
    src.close(mine, Some(copy.as_raw_fd()));
    let waker = Waker::from(Arc::clone(&woken));
    assert!(
        src.poll_drain(&mut Context::from_waker(&waker))
            .is_pending(),
        "the peer reads nothing, so the drain waits"
    );
    let mut after = After::default();
    held.release(&mut after, &src.hooks());
    after.run();
    assert!(
        woken.0.load(Ordering::SeqCst),
        "the drain waiting before the close was never woken"
    );
    drop(peer);
    src.close(copy, None);
    drop(rt);
}

/// An attach performed off the ring's thread hands the ring a copy of the
/// descriptor to put into its table. The handle closes before the ring
/// gets to it: the ring must not keep the file, or the peer never sees the
/// end. The ring's thread is held in a task meanwhile, so the order is
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
    // Letting go performs the attach here, which posts the copy to the ring.
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
