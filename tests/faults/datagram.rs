//! Datagrams under pressure: a flood from a socket with the smallest send
//! buffer into one with the smallest receive buffer, through every send and
//! receive call; a connected socket whose peer is gone. A datagram that
//! arrives arrives whole, once; one the kernel dropped is gone as std's
//! would be; no call answers `WouldBlock` or waits forever; and the sockets
//! work afterwards.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nuclei::net::UdpSocket;

use crate::support::*;

/// The sizes a flood cycles through: around each size class of the pool, the
/// threshold of owned datagrams, and up to near the largest UDP takes.
const SIZES: [usize; 14] = [
    16, 17, 100, 511, 512, 513, 1400, 4096, 12_287, 12_288, 12_289, 16_384, 32_768, 60_000,
];

const HEADER: usize = 16;
/// The sequence number of the datagram that says the flood is over.
const END: u32 = u32::MAX;

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// Datagram `seq` of `len` bytes: its number, length and checksum, then
/// bytes of its own.
fn datagram(seq: u32, len: usize) -> Vec<u8> {
    let len = len.max(HEADER);
    let payload = pattern(u64::from(seq), len - HEADER);
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&checksum(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// The number of a datagram that came whole, or a failure naming what is
/// wrong with it.
fn check(what: &str, got: &[u8]) -> u32 {
    assert!(
        got.len() >= HEADER,
        "{what}: a datagram of {} bytes",
        got.len()
    );
    let seq = u32::from_le_bytes(got[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(got[4..8].try_into().unwrap()) as usize;
    let sum = u64::from_le_bytes(got[8..16].try_into().unwrap());
    assert_eq!(got.len(), len, "{what}: datagram {seq} cut or grown");
    assert_eq!(
        checksum(&got[HEADER..]),
        sum,
        "{what}: datagram {seq} changed"
    );
    seq
}

#[derive(Clone, Copy, Debug)]
enum Send {
    To,
    Connected,
    Owned,
    ToOwned,
}

#[derive(Clone, Copy, Debug)]
enum Recv {
    From,
    Connected,
    Owned,
    FromOwned,
}

/// What a send may answer in a flood besides its count, as std's may: the
/// system short of buffers. Never `WouldBlock`: the runtime waits for room.
fn flood_error(err: &io::Error) -> bool {
    #[cfg(unix)]
    return err.raw_os_error() == Some(libc::ENOBUFS);
    #[cfg(windows)]
    return err.raw_os_error() == Some(10055);
}

async fn send_one(
    socket: &UdpSocket,
    how: Send,
    bytes: Vec<u8>,
    to: SocketAddr,
) -> io::Result<usize> {
    match how {
        Send::To => socket.send_to(&bytes, to).await,
        Send::Connected => socket.send(&bytes).await,
        Send::Owned => socket.send_owned(bytes).await.0,
        Send::ToOwned => socket.send_to_owned(bytes, to).await.0,
    }
}

async fn recv_one(socket: &UdpSocket, how: Recv) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; 65_536];
    match how {
        Recv::From => {
            let (n, _) = socket.recv_from(&mut buf).await?;
            buf.truncate(n);
            Ok(buf)
        }
        Recv::Connected => {
            let n = socket.recv(&mut buf).await?;
            buf.truncate(n);
            Ok(buf)
        }
        Recv::Owned => {
            let (n, buf) = socket.recv_owned(Vec::with_capacity(65_536)).await;
            n.map(|n| {
                assert_eq!(buf.len(), n);
                buf
            })
        }
        Recv::FromOwned => {
            let (n, buf) = socket.recv_from_owned(Vec::with_capacity(65_536)).await;
            n.map(|(n, _)| {
                assert_eq!(buf.len(), n);
                buf
            })
        }
    }
}

/// Floods `count` datagrams from a socket with the smallest send buffer to
/// one with the smallest receive buffer, sent `send`'s way and received
/// `recv`'s, and checks what came.
async fn flood(send: Send, recv: Recv, count: u32) {
    let what = format!("a flood of {count} sent {send:?}, received {recv:?}");
    let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    #[cfg(unix)]
    {
        set_tiny(tx.get_ref());
        set_tiny(rx.get_ref());
    }
    let (tx_addr, rx_addr) = (tx.local_addr().unwrap(), rx.local_addr().unwrap());
    tx.get_ref().connect(rx_addr).unwrap();
    rx.get_ref().connect(tx_addr).unwrap();
    let ended = Arc::new(AtomicBool::new(false));
    let receiver = {
        let ended = Arc::clone(&ended);
        let what = what.clone();
        nuclei::spawn(async move {
            let mut seen = vec![false; count as usize];
            let mut got = 0usize;
            loop {
                let bytes = within(&what, recv_one(&rx, recv)).await.unwrap();
                let seq = check(&what, &bytes);
                if seq == END {
                    ended.store(true, Ordering::Release);
                    return (got, rx);
                }
                let first = !core::mem::replace(&mut seen[seq as usize], true);
                assert!(first, "{what}: datagram {seq} came twice");
                got += 1;
            }
        })
    };
    let mut sent = 0usize;
    for seq in 0..count {
        let bytes = datagram(seq, SIZES[seq as usize % SIZES.len()]);
        let len = bytes.len();
        match within(&what, send_one(&tx, send, bytes, rx_addr)).await {
            Ok(n) => {
                assert_eq!(n, len, "{what}: datagram {seq} sent in part");
                sent += 1;
            }
            Err(err) => assert!(flood_error(&err), "{what}: datagram {seq}: {err}"),
        }
    }
    let mut pause = Duration::from_millis(1);
    within(&what, async {
        while !ended.load(Ordering::Acquire) {
            let _ = send_one(&tx, Send::To, datagram(END, HEADER), rx_addr).await;
            nuclei::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(50));
        }
    })
    .await;
    let (got, rx) = within(&what, receiver).await;
    assert!(got <= sent, "{what}: {got} datagrams came of {sent} sent");
    assert!(got > 0, "{what}: none of {sent} came");
    // Both sockets still work.
    while rx.try_recv(&mut [0u8; 16]).is_ok() {}
    let after = datagram(0, 100);
    within(&what, tx.send(&after)).await.unwrap();
    let back = within(&what, recv_one(&rx, Recv::From)).await.unwrap();
    assert!(
        back == after || check(&what, &back) == END,
        "{what}: the datagram after it"
    );
}

#[test]
fn a_flood_through_the_smallest_buffers_delivers_whole_datagrams_once() {
    if !alone("datagram::a_flood_through_the_smallest_buffers_delivers_whole_datagrams_once") {
        return;
    }
    let rt = runtime();
    rt.block_on(async {
        for (send, recv) in [
            (Send::To, Recv::From),
            (Send::Connected, Recv::Connected),
            (Send::Owned, Recv::Owned),
            (Send::ToOwned, Recv::FromOwned),
        ] {
            flood(send, recv, 3000).await;
        }
    });
    rt.block_on(round_trip());
}

#[test]
fn floods_hold_nothing() {
    let name = "datagram::floods_hold_nothing";
    if alone(name) {
        holds_steady(name, 3, || async {
            flood(Send::Owned, Recv::FromOwned, 500).await;
            flood(Send::To, Recv::Owned, 500).await;
        });
    }
}

/// Owned receives of two tasks at once on one socket, with a receive of the
/// driver's out for a borrowed one dropped before: each task gets its own
/// buffer back, with a datagram after the bytes it held.
async fn owned_receives_at_once() {
    for _ in 0..30 {
        owned_receives_at_once_on_a_new_socket().await;
    }
}

async fn owned_receives_at_once_on_a_new_socket() {
    use std::sync::Arc;
    let rx = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let to = rx.local_addr().unwrap();
    let mut buf = [0u8; 16];
    poll_times(
        "a borrowed receive",
        rx.recv_from(&mut buf),
        1,
        DropAt::Poll,
    )
    .await;
    let count = 20;
    let tasks: Vec<_> = (*b"AB")
        .into_iter()
        .map(|mark| {
            let rx = Arc::clone(&rx);
            nuclei::spawn_local(async move {
                let mut got = 0;
                while got < count / 2 {
                    let mut buf = Vec::with_capacity(64);
                    buf.extend_from_slice(&[mark, b':']);
                    let (n, back) = within("an owned receive", rx.recv_owned(buf)).await;
                    let n = n.unwrap();
                    assert_eq!(
                        &back[..2],
                        &[mark, b':'],
                        "a task got another task's buffer back"
                    );
                    assert_eq!(back.len(), n + 2, "an owned receive answered another count");
                    assert_eq!(&back[2..], b"datagram");
                    got += 1;
                }
            })
        })
        .collect();
    for _ in 0..count {
        tx.send_to(b"datagram", to).await.unwrap();
        nuclei::task::yield_now().await;
    }
    for task in tasks {
        within("the receiving tasks", task).await;
    }
}

#[test]
fn owned_receives_of_two_tasks_at_once_keep_their_own_buffers() {
    if alone("datagram::owned_receives_of_two_tasks_at_once_keep_their_own_buffers") {
        let rt = runtime();
        rt.block_on(owned_receives_at_once());
        rt.block_on(round_trip());
    }
}

/// A wake that only says it came.
struct Flag(AtomicBool);

impl std::task::Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A buffer of room for any datagram, holding `mark` and a colon first.
fn marked(mark: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 * 1024);
    buf.extend_from_slice(&[mark, b':']);
    buf
}

/// The owned receive of one task waits with its buffer lent while a datagram
/// the driver received ahead (into a buffer of its own, for a borrowed
/// receive out before) comes; a second task's owned receive then polls
/// before the first looks again. Each gets its own buffer back, with a
/// datagram after the bytes it held, and between them both datagrams.
async fn owned_receives_with_a_datagram_ahead() {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    let rx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let to = rx.local_addr().unwrap();
    let mut small = [0u8; 16];
    let mut borrowed = pin!(rx.recv_from(&mut small));
    let mut first = pin!(rx.recv_owned(marked(b'A')));
    let flag = Arc::new(Flag(AtomicBool::new(false)));
    let flagged = Waker::from(Arc::clone(&flag));
    let quiet = Waker::noop();
    assert!(
        borrowed
            .as_mut()
            .poll(&mut Context::from_waker(quiet))
            .is_pending(),
        "a borrowed receive with nothing sent"
    );
    // The driver's receive goes to the kernel before the datagram is sent.
    nuclei::time::sleep(Duration::from_millis(20)).await;
    tx.send_to(b"one", to).unwrap();
    // Polled at once, with no turn of the shard between: the datagram is in
    // the kernel and the driver's receive has not brought it yet.
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&flagged))
            .is_pending(),
        "an owned receive before the driver brought the datagram"
    );
    until("the datagram to reach the side", || {
        flag.0.load(Ordering::SeqCst)
    })
    .await;
    let mut second = pin!(rx.recv_owned(marked(b'B')));
    let early = second.as_mut().poll(&mut Context::from_waker(quiet));
    tx.send_to(b"two", to).unwrap();
    let (a, a_buf) = within("the first owned receive", first).await;
    let (b, b_buf) = match early {
        Poll::Ready(out) => out,
        Poll::Pending => within("the second owned receive", second).await,
    };
    assert!(
        a_buf.starts_with(b"A:"),
        "the first task got another task's buffer back: {a:?}, {:?}",
        String::from_utf8_lossy(&a_buf)
    );
    assert!(
        b_buf.starts_with(b"B:"),
        "the second task got another task's buffer back: {b:?}, {:?}",
        String::from_utf8_lossy(&b_buf)
    );
    assert_eq!(
        a_buf.len(),
        a.unwrap() + 2,
        "the first receive answered another count"
    );
    assert_eq!(
        b_buf.len(),
        b.unwrap() + 2,
        "the second receive answered another count"
    );
    let mut got = [a_buf[2..].to_vec(), b_buf[2..].to_vec()];
    got.sort();
    assert_eq!(got, [b"one".to_vec(), b"two".to_vec()]);
}

#[test]
fn owned_receives_with_a_datagram_ahead_keep_their_own_buffers() {
    if alone("datagram::owned_receives_with_a_datagram_ahead_keep_their_own_buffers") {
        let rt = runtime();
        rt.block_on(owned_receives_with_a_datagram_ahead());
        rt.block_on(round_trip());
    }
}

/// A connected socket whose peer's port is closed: the send goes, and the
/// next receive answers the refusal, as std's do.
#[test]
fn a_connected_socket_whose_peer_is_gone_answers_as_std() {
    if !alone("datagram::a_connected_socket_whose_peer_is_gone_answers_as_std") {
        return;
    }
    let gone = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = gone.local_addr().unwrap();
    drop(gone);
    let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    std_socket.connect(addr).unwrap();
    std_socket.set_read_timeout(Some(DEADLINE)).unwrap();
    let want_send = Answer::of(&std_socket.send(b"anyone"), |n| *n);
    let want_recv = Answer::of(&std_socket.recv(&mut [0u8; 16]), |n| *n);
    let rt = runtime();
    rt.block_on(async {
        for owned in [false, true] {
            let socket = UdpSocket::connect(addr).await.unwrap();
            let what = format!(
                "a {} send to a closed port",
                if owned { "owned" } else { "plain" }
            );
            let sent = if owned {
                within(&what, socket.send_owned(b"anyone".to_vec())).await.0
            } else {
                within(&what, socket.send(b"anyone")).await
            };
            assert_eq!(Answer::of(&sent, |n| *n), want_send, "{what}");
            let got = if owned {
                within(&what, socket.recv_owned(Vec::with_capacity(16)))
                    .await
                    .0
            } else {
                within(&what, socket.recv(&mut [0u8; 16])).await
            };
            assert_eq!(Answer::of(&got, |n| *n), want_recv, "{what}: the receive");
        }
    });
    rt.block_on(round_trip());
}

#[cfg(unix)]
mod unix {
    use nuclei::net::UnixDatagram;

    use super::{SIZES, check, datagram};
    use crate::support::*;

    /// A Unix datagram socket loses none: a flood into the smallest buffer
    /// waits for room, and every datagram arrives whole, once, in order.
    async fn flood(owned: bool, count: u32) {
        let what = format!("a Unix flood of {count}, owned {owned}");
        let (tx, rx) = UnixDatagram::pair().unwrap();
        set_tiny(tx.get_ref());
        set_tiny(rx.get_ref());
        let receiver = {
            let what = what.clone();
            nuclei::spawn(async move {
                for want in 0..count {
                    let got = if owned {
                        let (n, buf) =
                            within(&what, rx.recv_owned(Vec::with_capacity(65_536))).await;
                        assert_eq!(n.unwrap(), buf.len());
                        buf
                    } else {
                        let mut buf = vec![0u8; 65_536];
                        let n = within(&what, rx.recv(&mut buf)).await.unwrap();
                        buf.truncate(n);
                        buf
                    };
                    assert_eq!(check(&what, &got), want, "{what}: out of order");
                }
                rx
            })
        };
        for seq in 0..count {
            // A Unix datagram must fit the smallest send buffer whole.
            let bytes = datagram(seq, SIZES[seq as usize % SIZES.len()].min(4096));
            let len = bytes.len();
            let sent = if owned {
                within(&what, tx.send_owned(bytes)).await.0
            } else {
                within(&what, tx.send(&bytes)).await
            };
            assert_eq!(sent.unwrap(), len, "{what}: datagram {seq}");
        }
        within(&what, receiver).await;
    }

    /// Sends on a connected Unix datagram socket whose peer closed answer
    /// each what std's do: the kernel's own error for each datagram, not an
    /// earlier one's on a later send; a small datagram and one an owned send
    /// lends its buffer for alike.
    #[test]
    fn sends_to_a_closed_unix_datagram_peer_answer_as_std() {
        if !alone("datagram::unix::sends_to_a_closed_unix_datagram_peer_answer_as_std") {
            return;
        }
        use std::os::unix::net::UnixDatagram as StdDatagram;
        for len in [1, 16 * 1024] {
            let bytes = vec![b'x'; len];
            let (a, b) = StdDatagram::pair().unwrap();
            drop(b);
            let want: Vec<Answer> = (0..3)
                .map(|_| Answer::of(&a.send(&bytes), |n| *n))
                .collect();
            let rt = runtime();
            rt.block_on(async {
                for owned in [false, true] {
                    let (a, b) = UnixDatagram::pair().unwrap();
                    drop(b);
                    let mut got = Vec::new();
                    for _ in 0..3 {
                        let sent = if owned {
                            within("a send to a closed peer", a.send_owned(bytes.clone()))
                                .await
                                .0
                        } else {
                            within("a send to a closed peer", a.send(&bytes)).await
                        };
                        got.push(Answer::of(&sent, |n| *n));
                    }
                    assert_eq!(
                        got, want,
                        "sends of {len} bytes to a closed peer, owned {owned}"
                    );
                }
            });
        }
    }

    #[test]
    fn a_unix_flood_waits_for_room_and_loses_none() {
        if alone("datagram::unix::a_unix_flood_waits_for_room_and_loses_none") {
            let rt = runtime();
            rt.block_on(async {
                flood(false, 3000).await;
                flood(true, 3000).await;
            });
            rt.block_on(round_trip());
        }
    }

    #[test]
    fn unix_floods_hold_nothing() {
        let name = "datagram::unix::unix_floods_hold_nothing";
        if alone(name) {
            holds_steady(name, 3, || async {
                flood(true, 400).await;
                flood(false, 400).await;
            });
        }
    }
}
