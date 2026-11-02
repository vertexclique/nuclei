//! A caller's clear of a side's readiness that finds the side held by another (another task's
//! look, the driver's edge): the holder clears the side as it lets go, so the caller's next
//! look waits for the next edge (proofs/tla/asyncfd/AsyncFd.tla, the `drop_held_clear`
//! mutation).

use super::*;
use crate::io::side::After;

/// Whether a look at the read side now waits.
fn read_waits(src: &Arc<Source>) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    src.poll_fd_read_ready(&mut cx).is_pending()
}

/// Whether a look at the write side now waits.
fn write_waits(src: &Arc<Source>) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    src.poll_fd_write_ready(&mut cx).is_pending()
}

#[test]
fn a_read_clear_made_while_the_side_is_held_is_kept() {
    crate::block_on(async {
        let (mine, peer) = UnixStream::pair().unwrap();
        let mine = Owned::new(mine, Kind::Fd);
        let src = &mine.src;
        let tick = within(poll_fn(|cx| src.poll_fd_read_ready(cx)))
            .await
            .unwrap();
        // The caller's call found nothing to read; its clear comes while another holds the side.
        let held = src.read.try_hold().expect("the side is free");
        src.fd_clear_read(tick);
        let mut after = After::default();
        held.release(&mut after, &src.hooks());
        after.run();
        assert!(
            read_waits(src),
            "the clear made while the side was held was lost"
        );
        (&peer).write_all(b"x").unwrap();
        let next = within(poll_fn(|cx| src.poll_fd_read_ready(cx)))
            .await
            .unwrap();
        assert_ne!(next, tick, "the side came ready with no edge");
        let mut byte = [0u8; 1];
        assert_eq!(mine.io().read(&mut byte).unwrap(), 1);
        assert_eq!(&byte, b"x");
    });
}

#[test]
fn a_write_clear_made_while_the_side_is_held_is_kept() {
    crate::block_on(async {
        let (mine, mut peer) = UnixStream::pair().unwrap();
        let mine = Owned::new(mine, Kind::Fd);
        let src = &mine.src;
        let tick = within(poll_fn(|cx| src.poll_fd_write_ready(cx)))
            .await
            .unwrap();
        let chunk = [7u8; 4096];
        let mut sent = 0;
        loop {
            match mine.io().write(&chunk) {
                Ok(n) => sent += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("{err}"),
            }
        }
        let held = src.write.try_hold().expect("the side is free");
        src.fd_clear_write(tick);
        let mut after = After::default();
        held.release(&mut after, &src.hooks());
        after.run();
        assert!(
            write_waits(src),
            "the clear made while the side was held was lost"
        );
        let mut got = vec![0u8; sent];
        peer.read_exact(&mut got).unwrap();
        assert!(got.iter().all(|&b| b == 7), "the peer read other bytes");
        let next = within(poll_fn(|cx| src.poll_fd_write_ready(cx)))
            .await
            .unwrap();
        assert_ne!(next, tick, "the side came ready with no edge");
    });
}

#[test]
fn clears_left_together_keep_the_newest() {
    let left = super::super::clear::LeftClear::default();
    assert_eq!(left.take(), None);
    left.leave(5);
    left.leave(4);
    assert_eq!(left.take(), Some(5), "an older tick replaced a newer one");
    assert_eq!(left.take(), None, "a tick was taken twice");
    left.leave(u32::MAX);
    left.leave(0);
    assert_eq!(left.take(), Some(0), "the count's wrap was taken as older");
    left.leave(u32::MAX);
    assert_eq!(left.take(), Some(u32::MAX), "a tick of all ones was lost");
}
