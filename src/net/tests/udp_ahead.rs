//! A datagram a look came to before its receive: a look at the sender, or a peek into a buffer
//! smaller than the datagram, leaves it whole for the receive after, on every backend. A port
//! receives a datagram ahead into a buffer of the pool for the call that waits, and once cut one
//! to that call's length, so the receive after got its first bytes as a success.

use std::net::UdpSocket;

use super::{runtime, within};
use crate::io::Handle;

/// Datagram sizes past the pool's smallest buffer (512 B), up to what every system sends by
/// default (macOS caps a UDP datagram at 9216 B).
const SIZES: [usize; 3] = [513, 1400, 9000];

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(13) ^ (i >> 8) as u8 ^ seed)
        .collect()
}

#[test]
fn a_datagram_comes_whole_after_a_look_at_its_sender_or_a_short_peek() {
    let rt = runtime();
    rt.block_on(async {
        let a = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
        let b = Handle::<UdpSocket>::bind("127.0.0.1:0").await.unwrap();
        let (from, to) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        let mut got = vec![0u8; 64 * 1024];
        for (seed, &len) in SIZES.iter().enumerate() {
            let sent = pattern(len, seed as u8);
            // The look waits first, so the driver takes the datagram for it.
            let (sender, wrote) = within(super::join(b.peek_sender(), async {
                crate::time::sleep(std::time::Duration::from_millis(5)).await;
                a.send_to(&sent, to).await
            }))
            .await;
            assert_eq!((sender.unwrap(), wrote.unwrap()), (from, len));
            let mut short = [0u8; 16];
            let peeked = within(b.peek_from(&mut short)).await.unwrap();
            assert_eq!(peeked, (16, from));
            assert_eq!(short, sent[..16]);
            let (n, sender) = within(b.recv_from(&mut got)).await.unwrap();
            assert_eq!((n, sender), (len, from), "a datagram of {len} bytes");
            assert!(got[..n] == sent[..], "the datagram of {len} bytes differs");
        }
    });
}
