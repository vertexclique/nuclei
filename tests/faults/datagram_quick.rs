//! A datagram call made at once, with no guard, only while its side holds
//! nothing (proofs/tla/dgram): a datagram the side keeps from a dropped
//! owned receive comes before the next one the kernel holds, and a send that
//! finds the socket full waits for room and is woken by it.

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Waker};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nuclei::net::UdpSocket;

use crate::support::*;

/// A wake that only says it came.
struct Flag(AtomicBool);

impl std::task::Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Room for any datagram these cases send, past the size below which a
/// ring's owned receive copies rather than lends its buffer.
const ROOM: usize = 16 * 1024;

/// Polls `receive` once with a flagged waker, sends `one` and `two` with
/// `send`, waits for the wake that says the driver received `one` into the
/// receive's buffer, and drops it unread.
async fn dropped_holding_one<F: Future>(receive: F, send: impl Fn(&[u8])) {
    let mut receive = pin!(receive);
    let flag = Arc::new(Flag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&flag));
    assert!(
        receive
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending(),
        "an owned receive with nothing sent"
    );
    send(b"one");
    send(b"two");
    until(
        "the first datagram to reach the owned receive's buffer",
        || flag.0.load(Ordering::SeqCst),
    )
    .await;
}

#[test]
fn a_udp_datagram_a_dropped_owned_receive_held_comes_first() {
    let rt = runtime();
    rt.block_on(async {
        let rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = rx.local_addr().unwrap();
        dropped_holding_one(rx.recv_owned(Vec::with_capacity(ROOM)), |d| {
            tx.send_to(d, to).unwrap();
        })
        .await;
        let mut buf = [0u8; 64];
        for want in [&b"one"[..], b"two"] {
            let (n, from) = within("a borrowed receive", rx.recv_from(&mut buf))
                .await
                .unwrap();
            assert_eq!((&buf[..n], from), (want, tx.local_addr().unwrap()));
        }
    });
}

#[cfg(unix)]
#[test]
fn a_unix_datagram_a_dropped_owned_receive_held_comes_first() {
    let rt = runtime();
    rt.block_on(async {
        let (tx, rx) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let rx = nuclei::Handle::new(rx).unwrap();
        dropped_holding_one(rx.recv_owned(Vec::with_capacity(ROOM)), |d| {
            tx.send(d).unwrap();
        })
        .await;
        let mut buf = [0u8; 64];
        for want in [&b"one"[..], b"two"] {
            let (n, from) = within("a borrowed receive", rx.recv_from(&mut buf))
                .await
                .unwrap();
            assert_eq!(&buf[..n], want);
            assert!(from.is_unnamed());
        }
    });
}

/// Linux answers a send on a Unix datagram socket whose buffer its datagrams
/// in the peer's queue fill "not now", and says it may send again once they
/// take at most a quarter of it; macOS answers it with an error.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_unix_datagram_send_to_a_full_peer_waits_for_room() {
    let rt = runtime();
    rt.block_on(async {
        let (tx, rx) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let tx = nuclei::Handle::new(tx).unwrap();
        let mut filled = 0;
        loop {
            match tx.try_send(b"fill") {
                Ok(_) => filled += 1,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("a send to a peer with room: {err}"),
            }
        }
        assert!(filled > 0, "the peer took no datagram");
        let mut send = pin!(tx.send(b"last"));
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        assert!(
            send.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending(),
            "a send to a full peer"
        );
        // The peer takes every datagram queued: the socket says it may send.
        rx.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 16];
        let mut taken = 0;
        while let Ok(n) = rx.recv(&mut buf) {
            assert_eq!(&buf[..n], b"fill");
            taken += 1;
        }
        assert_eq!(taken, filled);
        until("the room the peer made to wake the send", || {
            flag.0.load(Ordering::SeqCst)
        })
        .await;
        assert_eq!(within("the send after room came", send).await.unwrap(), 4);
        rx.set_nonblocking(false).unwrap();
        assert_eq!(rx.recv(&mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"last");
    });
}
