//! A datagram longer than the caller's buffer: on unix the bytes that fit, cut, as a success, as
//! std's receive answers; on Windows `WSAEMSGSIZE`, as std's does, the buffer holding the bytes
//! that fit and the datagram consumed (a peek leaves it). The datagram after it comes whole.

use std::io;
use std::net::UdpSocket;

use super::{runtime, within};
use crate::io::Handle;

const LONG: usize = 100;
const SHORT: usize = 10;
#[cfg(windows)]
const WSAEMSGSIZE: i32 = 10040;

fn pattern(seed: u8) -> Vec<u8> {
    (0..LONG)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

/// Checks the answer of a receive that cut a datagram to `SHORT` bytes.
fn cut_answer<T: core::fmt::Debug>(answer: io::Result<T>, ok: impl FnOnce(T)) {
    #[cfg(unix)]
    ok(answer.unwrap());
    #[cfg(windows)]
    {
        let _ = ok;
        let err = answer.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(WSAEMSGSIZE), "{err:?}");
    }
}

async fn pair() -> (Handle<UdpSocket>, Handle<UdpSocket>) {
    let a = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
    let b = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
    (a, b)
}

/// The datagram after a cut one, received whole and compared byte for byte.
async fn second_comes_whole(a: &Handle<UdpSocket>, b: &Handle<UdpSocket>, seed: u8) {
    let to = b.local_addr().unwrap();
    let sent = pattern(seed);
    assert_eq!(within(a.send_to(&sent, to)).await.unwrap(), LONG);
    let mut got = vec![0u8; 2 * LONG];
    let (n, from) = within(b.recv_from(&mut got)).await.unwrap();
    assert_eq!((n, from), (LONG, a.local_addr().unwrap()));
    assert!(got[..n] == sent[..], "the second datagram differs");
}

#[test]
fn recv_from_cuts_a_long_datagram_as_std_does() {
    let rt = runtime();
    rt.block_on(async {
        let (a, b) = pair().await;
        let sent = pattern(1);
        within(a.send_to(&sent, b.local_addr().unwrap()))
            .await
            .unwrap();
        let mut small = [0u8; SHORT];
        let from = a.local_addr().unwrap();
        cut_answer(within(b.recv_from(&mut small)).await, |(n, f)| {
            assert_eq!((n, f), (SHORT, from));
        });
        assert!(small[..] == sent[..SHORT], "the bytes that fit differ");
        second_comes_whole(&a, &b, 2).await;
    });
}

#[test]
fn peek_from_cuts_a_long_datagram_and_leaves_it() {
    let rt = runtime();
    rt.block_on(async {
        let (a, b) = pair().await;
        let sent = pattern(3);
        within(a.send_to(&sent, b.local_addr().unwrap()))
            .await
            .unwrap();
        let mut small = [0u8; SHORT];
        let from = a.local_addr().unwrap();
        cut_answer(within(b.peek_from(&mut small)).await, |(n, f)| {
            assert_eq!((n, f), (SHORT, from));
        });
        assert!(small[..] == sent[..SHORT], "the bytes that fit differ");
        let mut got = vec![0u8; 2 * LONG];
        let (n, _) = within(b.recv_from(&mut got)).await.unwrap();
        assert!(got[..n] == sent[..], "the peek took the datagram");
        second_comes_whole(&a, &b, 4).await;
    });
}

#[test]
fn recv_from_owned_cuts_a_long_datagram_as_std_does() {
    let rt = runtime();
    rt.block_on(async {
        let (a, b) = pair().await;
        let sent = pattern(5);
        let to = b.local_addr().unwrap();
        // The receive waits first, so the driver receives into the lent buffer.
        let recv = b.recv_from_owned(Vec::with_capacity(SHORT));
        let send = async {
            crate::time::sleep(std::time::Duration::from_millis(5)).await;
            a.send_to(&sent, to).await
        };
        let ((answer, buf), wrote) = within(super::join(recv, send)).await;
        assert_eq!(wrote.unwrap(), LONG);
        cut_answer(answer, |(n, _)| assert_eq!(n, SHORT));
        assert!(buf[..] == sent[..SHORT], "the bytes that fit differ");
        // One that came before the receive is copied in, and cut the same way.
        within(a.send_to(&sent, to)).await.unwrap();
        within(b.readable()).await.unwrap();
        let (answer, buf) = within(b.recv_from_owned(Vec::with_capacity(SHORT))).await;
        cut_answer(answer, |(n, _)| assert_eq!(n, SHORT));
        assert!(buf[..] == sent[..SHORT], "the bytes that fit differ");
        second_comes_whole(&a, &b, 6).await;
    });
}
