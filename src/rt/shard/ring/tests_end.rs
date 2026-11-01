//! A stream's end queued with bytes before its multishot receive is armed: the kernel's arming
//! look takes the bytes and, the socket's count at none, waits for a wake the end gave already.
//! The test hook has the kernel skip the receive's first try and arm it at once, as it does when
//! the bytes and the end come between that try and the arming, so the window is open every time;
//! each end must still reach the reader, after the bytes, as std's reads see it
//! (proofs/tla/ring/RingEnd.tla).

use core::future::poll_fn;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use super::tests_recv::{ring_shard_of, source_pair};
use crate::io::source::ring::POLL_FIRST_FOR_TEST;
use crate::io::source::{Kind, Source};
use crate::io::sys;
use crate::rt::context;

/// How the peer ends its stream after its bytes.
#[derive(Clone, Copy, Debug)]
enum End {
    /// A Unix stream closed with a byte of ours unread: a reset.
    UnixReset,
    /// A Unix stream closed with nothing unread.
    UnixClose,
    /// A TCP stream closed with `SO_LINGER` 0.
    TcpReset,
}

/// Turns the hook off as the test ends, however it ends.
struct PollFirst;

impl PollFirst {
    fn on() -> Self {
        POLL_FIRST_FOR_TEST.with(|f| f.set(true));
        Self
    }
}

impl Drop for PollFirst {
    fn drop(&mut self) {
        POLL_FIRST_FOR_TEST.with(|f| f.set(false));
    }
}

/// A connected TCP pair of std's: ours, then the peer.
fn std_tcp() -> (std::net::TcpStream, std::net::TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mine, _) = listener.accept().unwrap();
    (mine, peer)
}

/// A TCP pair: ours non-blocking with its source, and the peer.
fn tcp_pair() -> (std::net::TcpStream, Arc<Source>, std::net::TcpStream) {
    let (mine, peer) = std_tcp();
    sys::set_nonblocking(mine.as_raw_fd()).unwrap();
    let src = Source::new(mine.as_raw_fd(), Kind::Tcp);
    (mine, src, peer)
}

/// What std's reads of `bytes` then `end` answer: the bytes, then each read after them.
fn std_answers(end: End, bytes: &[u8]) -> (Vec<u8>, Vec<Option<io::ErrorKind>>) {
    let mut ours: Box<dyn Read> = match end {
        End::UnixReset | End::UnixClose => {
            let (mut ours, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
            if matches!(end, End::UnixReset) {
                ours.write_all(b"x").unwrap();
            }
            peer.write_all(bytes).unwrap();
            drop(peer);
            Box::new(ours)
        }
        End::TcpReset => {
            let (ours, mut peer) = std_tcp();
            peer.write_all(bytes).unwrap();
            linger_zero(&peer);
            drop(peer);
            Box::new(ours)
        }
    };
    let mut got = vec![0u8; bytes.len()];
    ours.read_exact(&mut got).unwrap();
    let after = (0..2)
        .map(|_| ours.read(&mut [0u8; 4096]).err().map(|e| e.kind()))
        .collect();
    (got, after)
}

/// `SO_LINGER` 0, so the close resets the connection.
fn linger_zero(s: &std::net::TcpStream) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: a plain setsockopt with a pointer to our own struct.
    let rc = unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&linger as *const libc::linger).cast(),
            size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "{}", io::Error::last_os_error());
}

/// Bytes, then `end`, both queued before the first read arms the receive: the reader gets the
/// bytes and then what std's reads get after them, each read within a deadline.
fn end_after_bytes_reaches_the_reader(end: End) {
    let Some(shard) = ring_shard_of(64, 16, 4096) else {
        return;
    };
    let _current = context::enter(&shard);
    let bytes: Vec<u8> = (0..1000u32).map(|i| (i * 7 + i / 251) as u8).collect();
    let (want, want_after) = std_answers(end, &bytes);
    let (_mine, src) = match end {
        End::UnixReset | End::UnixClose => {
            let (mut mine, src, mut peer) = source_pair();
            if matches!(end, End::UnixReset) {
                mine.write_all(b"x").unwrap();
            }
            peer.write_all(&bytes).unwrap();
            drop(peer);
            (Box::new(mine) as Box<dyn AsRawFd>, src)
        }
        End::TcpReset => {
            let (mine, src, mut peer) = tcp_pair();
            peer.write_all(&bytes).unwrap();
            linger_zero(&peer);
            drop(peer);
            (Box::new(mine) as Box<dyn AsRawFd>, src)
        }
    };
    let _hook = PollFirst::on();
    shard.block_on(async {
        let read = |what: &'static str| {
            let src = Arc::clone(&src);
            async move {
                let mut buf = vec![0u8; 4096];
                let answer = crate::time::timeout(
                    Duration::from_secs(10),
                    poll_fn(|cx| src.poll_read(cx, &mut buf, false)),
                )
                .await
                .unwrap_or_else(|_| panic!("{end:?}: {what}: no answer in 10 s"));
                answer.map(|n| buf[..n].to_vec())
            }
        };
        let mut got = Vec::new();
        while got.len() < want.len() {
            let more = read("a read of the bytes").await.unwrap();
            assert!(
                !more.is_empty(),
                "{end:?}: the stream ended after {} bytes",
                got.len()
            );
            got.extend_from_slice(&more);
        }
        assert!(got == want, "{end:?}: the bytes changed");
        for (i, want) in want_after.iter().enumerate() {
            let answer = read("a read after the bytes").await;
            match (&answer, want) {
                (Ok(rest), None) => assert!(rest.is_empty(), "{end:?}: bytes after the end"),
                (Err(err), Some(kind)) => assert_eq!(err.kind(), *kind, "{end:?}: read {i}"),
                _ => panic!("{end:?}: read {i} after the bytes: {answer:?}, std's {want:?}"),
            }
        }
    });
    src.retire();
}

#[test]
fn a_unix_reset_queued_with_bytes_before_the_receive_is_armed_reaches_the_reader() {
    end_after_bytes_reaches_the_reader(End::UnixReset);
}

#[test]
fn a_unix_close_queued_with_bytes_before_the_receive_is_armed_reaches_the_reader() {
    end_after_bytes_reaches_the_reader(End::UnixClose);
}

#[test]
fn a_tcp_reset_queued_with_bytes_before_the_receive_is_armed_reaches_the_reader() {
    end_after_bytes_reaches_the_reader(End::TcpReset);
}
