//! A connection its peer resets while it waits in the listener's backlog: every way of accepting
//! skips it and takes the next one, with every byte that one's peer sent, and goes on doing so.
//! Linux hands such a connection over as it is, its first read telling the reset; whether
//! another system does depends on when the reset reached it, so a connection handed over from
//! the peer that reset is let by, once, and nothing else is.

use core::future::{Future, poll_fn};
use core::task::Poll;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

use super::{next, read_exact, read_some, runtime, within};
use crate::io::Handle;
use crate::runtime::spawn;

/// The bytes a kept peer sends, more than one read takes.
const SENT: usize = 256 * 1024;

fn pattern(seed: u8) -> Vec<u8> {
    (0..SENT)
        .map(|i| (i as u8).wrapping_mul(31) ^ (i >> 8) as u8 ^ seed)
        .collect()
}

/// A peer that connects and resets at once (zero linger), before any accept takes its
/// connection: the address it connected from.
fn reset_peer(at: SocketAddr) -> SocketAddr {
    let peer = TcpStream::connect(at).unwrap();
    let from = peer.local_addr().unwrap();
    #[cfg(unix)]
    let raw = std::os::fd::AsRawFd::as_raw_fd(&peer);
    #[cfg(windows)]
    let raw = std::os::windows::io::AsRawSocket::as_raw_socket(&peer);
    crate::net::opt::set_linger_time(raw, Some(Duration::ZERO)).unwrap();
    drop(peer);
    from
}

/// A peer that connects after it and sends [`pattern`] of `seed`, then ends its stream: the
/// address it connected from, and its sender.
fn kept_peer(at: SocketAddr, seed: u8) -> (SocketAddr, JoinHandle<()>) {
    let mut peer = TcpStream::connect(at).unwrap();
    let from = peer.local_addr().unwrap();
    let sender = std::thread::spawn(move || {
        peer.write_all(&pattern(seed)).unwrap();
        peer.shutdown(Shutdown::Write).unwrap();
        // Open until the listener's side read the end, so nothing is reset under it.
        let mut end = [0u8; 1];
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _ = peer.read(&mut end);
    });
    (from, sender)
}

/// The peer an accepted connection came from, `None` when the system cannot say (a connection
/// reset before it was taken, on Linux).
fn peer_of(conn: &Handle<TcpStream>) -> Option<SocketAddr> {
    conn.peer_addr().ok()
}

/// A connection the accept handed over that is not the kept peer's: it must be the one of the
/// peer that reset, which the kernel may hand over as it is, once.
fn let_by(peer: Option<SocketAddr>, reset: SocketAddr, handed: &mut usize) {
    assert!(
        peer.is_none_or(|peer| peer == reset),
        "a connection from {peer:?}, which no peer of the test made"
    );
    *handed += 1;
    assert!(*handed <= 1, "the reset connection was handed over twice");
}

/// Every byte of the kept peer's connection, then its end.
async fn check_kept(mut conn: Handle<TcpStream>, seed: u8, sender: JoinHandle<()>) {
    let mut got = vec![0u8; SENT];
    within(read_exact(&mut conn, &mut got)).await.unwrap();
    assert!(got == pattern(seed), "the kept connection's bytes differ");
    let mut end = [0u8; 1];
    assert_eq!(within(read_some(&mut conn, &mut end)).await.unwrap(), 0);
    drop(conn);
    sender.join().unwrap();
}

/// The connections the listeners of `rt`'s workers skipped.
#[cfg(target_os = "linux")]
fn skipped_by(rt: &crate::runtime::Runtime) -> u64 {
    crate::stats()
        .shards
        .iter()
        .filter(|shard| shard.runtime == Some(rt.id()))
        .map(|shard| shard.errors.accepts_skipped)
        .sum()
}

#[test]
fn accept_skips_a_connection_reset_in_the_backlog() {
    let rt = runtime();
    // On a worker of this runtime alone, so the skips counted are its own.
    let handed_over = rt.block_on(spawn(async {
        let mut handed_over = 0;
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        for seed in 0..3 {
            // The accept waits before the peers come in the second round, and after them in the
            // others: on a completion driver one is out as the reset comes.
            let mut early = (seed == 1).then(|| Box::pin(listener.accept()));
            if let Some(accept) = early.as_mut() {
                let pending = poll_fn(|cx| Poll::Ready(accept.as_mut().poll(cx).is_pending()));
                assert!(pending.await, "an accept with no peer yet ended");
            }
            let reset = reset_peer(at);
            let (kept, sender) = kept_peer(at, seed);
            let mut handed = 0;
            let conn = loop {
                let (conn, peer) = match early.take() {
                    Some(accept) => within(accept).await,
                    None => within(listener.accept()).await,
                }
                .unwrap();
                if peer == kept {
                    break conn;
                }
                let_by(Some(peer), reset, &mut handed);
            };
            handed_over += handed;
            check_kept(conn, seed, sender).await;
        }
        handed_over
    }));
    // Linux names no peer for a connection reset before it was taken and answers `ECONNABORTED`
    // (the listener skips it), unless it hands the connection over as it is: each reset
    // connection is one or the other. Elsewhere the system may drop it from the backlog unseen.
    #[cfg(target_os = "linux")]
    assert_eq!(skipped_by(&rt), 3 - handed_over as u64, "skips counted");
    #[cfg(not(target_os = "linux"))]
    let _ = handed_over;
}

#[test]
fn incoming_skips_a_connection_reset_in_the_backlog() {
    let rt = runtime();
    rt.block_on(async {
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let mut incoming = listener.incoming();
        for seed in 0..2 {
            let reset = reset_peer(at);
            let (kept, sender) = kept_peer(at, seed);
            let mut handed = 0;
            let conn = loop {
                let conn = within(next(&mut incoming)).await.unwrap().unwrap();
                let peer = peer_of(&conn);
                if peer == Some(kept) {
                    break conn;
                }
                let_by(peer, reset, &mut handed);
            };
            check_kept(conn, seed, sender).await;
        }
    });
}

#[test]
fn accept_multi_skips_a_connection_reset_in_the_backlog_and_goes_on() {
    let rt = runtime();
    rt.block_on(async {
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let mut multi = within(listener.accept_multi()).await.unwrap();
        for seed in 0..3 {
            let reset = reset_peer(at);
            let (kept, sender) = kept_peer(at, seed);
            let mut handed = 0;
            let conn = loop {
                let Some(conn) = within(next(&mut multi)).await else {
                    panic!("the stream ended: {:?}", multi.take_error());
                };
                let peer = peer_of(&conn);
                if peer == Some(kept) {
                    break conn;
                }
                let_by(peer, reset, &mut handed);
            };
            check_kept(conn, seed, sender).await;
        }
        assert!(multi.take_error().is_none());
    });
}
