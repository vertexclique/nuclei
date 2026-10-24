//! Requests that need both sides: the test holds a side itself, so the
//! request cannot run at once, and checks that nothing waits, that the work
//! is done by whoever lets go last, and that a descriptor closes only then.

use super::*;
use crate::io::Handle;
use crate::io::side::After;

/// The peer of a socket, polled once: still open (nothing to read), or at
/// its end.
fn peer_sees_end(peer: &UnixStream) -> bool {
    let mut buf = [0u8; 1];
    match (&*peer).read(&mut buf) {
        Ok(0) => true,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_handle_dropped_while_a_side_is_held_closes_once_the_holder_lets_go() {
    let (mine, peer) = UnixStream::pair().unwrap();
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
    let (mine, peer) = UnixStream::pair().unwrap();
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
    let (mine, _peer) = UnixStream::pair().unwrap();
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
    let (mine, peer) = UnixStream::pair().unwrap();
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
