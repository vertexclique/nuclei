//! `TCP_INQ` and a Unix stream's `SO_INQ`: what a read of the socket leaves is
//! counted by the kernel, so a read that filled its buffer still knows whether
//! the socket is empty, and a kernel that refuses the option leaves the side on
//! the read-ahead path.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;

use super::*;
use crate::io::side::After;
use crate::io::source::read::ReadState;
use crate::io::source::{Io, Via};

fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

/// Waits until the peer's end of the stream reached `fd`.
fn wait_for_end(fd: Raw) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLRDHUP,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`.
        unsafe { libc::poll(&mut poll, 1, 10) };
        if poll.revents & libc::POLLRDHUP != 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the end of the stream never came"
        );
    }
}

#[test]
fn a_read_counts_what_it_leaves() {
    let (mut client, server) = tcp_pair();
    assert!(
        sys::enable_inq(server.as_raw_fd(), true),
        "TCP_INQ is refused"
    );
    client.write_all(&[7u8; 100]).unwrap();
    let mut buf = [0u8; 64];
    let (n, left) = sys::read_inq(server.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(n, 64);
    assert_eq!(left, Some(36));
    let (n, left) = sys::read_inq(server.as_raw_fd(), &mut buf).unwrap();
    assert_eq!((n, left), (36, Some(0)));
}

#[test]
fn an_exact_read_that_empties_the_socket_says_so() {
    let (mut client, server) = tcp_pair();
    assert!(
        sys::enable_inq(server.as_raw_fd(), true),
        "TCP_INQ is refused"
    );
    client.write_all(&[1u8; 64]).unwrap();
    let mut buf = [0u8; 64];
    let (n, left) = sys::read_inq(server.as_raw_fd(), &mut buf).unwrap();
    assert_eq!((n, left), (64, Some(0)));
}

#[test]
fn a_pending_end_of_stream_is_not_an_empty_socket() {
    let (mut client, server) = tcp_pair();
    assert!(
        sys::enable_inq(server.as_raw_fd(), true),
        "TCP_INQ is refused"
    );
    client.write_all(&[1u8; 64]).unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    wait_for_end(server.as_raw_fd());
    let mut buf = [0u8; 64];
    let (n, left) = sys::read_inq(server.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(n, 64);
    assert_ne!(left, Some(0), "the end of the stream is still to be read");
    let (n, _) = sys::read_inq(server.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(n, 0);
}

/// Waits until the socket `fd` holds `len` bytes to read.
fn wait_for_bytes(fd: Raw, len: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut held: libc::c_int = 0;
        // SAFETY: `FIONREAD` writes one `int`, ours.
        assert_eq!(unsafe { libc::ioctl(fd, libc::FIONREAD, &mut held) }, 0);
        if usize::try_from(held).is_ok_and(|held| held >= len) {
            return;
        }
        assert!(Instant::now() < deadline, "the bytes never came");
        std::thread::yield_now();
    }
}

/// A poller's direct read of `out` from the TCP socket `fd`, as a task's read makes it, with
/// whether it asked the kernel to count what the read leaves pushed onto `asked`. `None` when
/// the read was not made there (the side waits for an edge).
fn read_direct(
    st: &mut ReadState,
    fd: Raw,
    out: &mut [u8],
    asked: &mut Vec<bool>,
) -> Option<io::Result<usize>> {
    let io = Io {
        via: Via::Fd(fd),
        shard: None,
    };
    let len = out.len();
    let poll = st.read_direct(
        io,
        Some(fd),
        Kind::Tcp,
        len,
        false,
        Waker::noop(),
        |fd, count| {
            asked.push(count);
            if count {
                sys::read_inq(fd, out)
            } else {
                sys::read(fd, out).map(|n| (n, None))
            }
        },
    )?;
    match poll {
        Poll::Ready(result) => Some(result),
        Poll::Pending => panic!("a poller's direct read never waits"),
    }
}

#[test]
fn a_read_the_last_count_covers_asks_for_no_count() {
    let (mut client, server) = tcp_pair();
    let fd = server.as_raw_fd();
    let mut st = ReadState::default();
    let bytes: Vec<u8> = (0..=255).collect();
    client.write_all(&bytes).unwrap();
    wait_for_bytes(fd, bytes.len());
    let mut asked = Vec::new();
    for piece in bytes.chunks(64) {
        let mut out = [0u8; 64];
        let n = read_direct(&mut st, fd, &mut out, &mut asked).map(Result::unwrap);
        assert_eq!(n, Some(64));
        assert_eq!(out, piece);
    }
    // The first read counts 192 left; the two the count covers ask for none; the last, which
    // takes what the count left, asks again and learns the socket is empty.
    assert_eq!(asked, [true, false, false, true]);
    let mut out = [0u8; 64];
    assert!(
        read_direct(&mut st, fd, &mut out, &mut asked).is_none(),
        "an empty socket waits for its edge with no call"
    );
    assert_eq!(asked.len(), 4);
}

#[test]
fn a_floor_others_took_from_is_forgotten_once_the_socket_is_empty() {
    let (mut client, mut server) = tcp_pair();
    let fd = server.as_raw_fd();
    let mut st = ReadState::default();
    client.write_all(&[5u8; 192]).unwrap();
    wait_for_bytes(fd, 192);
    let mut asked = Vec::new();
    let mut out = [0u8; 64];
    let n = read_direct(&mut st, fd, &mut out, &mut asked).map(Result::unwrap);
    assert_eq!(n, Some(64));
    // Another read takes what the count said is left: the floor is above what the socket holds.
    let mut rest = [0u8; 128];
    server.read_exact(&mut rest).unwrap();
    server.set_nonblocking(true).unwrap();
    assert!(
        read_direct(&mut st, fd, &mut out, &mut asked).is_none(),
        "the read finds the socket empty and waits for its edge"
    );
    assert_eq!(asked, [true, false]);
    // The next edge, and bytes: the floor is gone, so the read asks for a count again.
    let mut after = After::default();
    st.serve(fd, Kind::Tcp, true, &mut after);
    drop(after);
    client.write_all(&[6u8; 64]).unwrap();
    wait_for_bytes(fd, 64);
    let n = read_direct(&mut st, fd, &mut out, &mut asked).map(Result::unwrap);
    assert_eq!(n, Some(64));
    assert_eq!(out, [6u8; 64]);
    assert_eq!(asked, [true, false, true]);
}

#[test]
fn a_socket_that_refuses_the_option_falls_back() {
    // A Unix socket refuses `TCP_INQ` as the TCP socket of an old kernel does.
    let (a, _b) = UnixStream::pair().unwrap();
    let mut st = ReadState::default();
    assert!(st.inq_wanted(Kind::Tcp), "the option is tried first");
    assert_eq!(
        st.inq_wanted(Kind::Stream),
        sys::UNIX_INQ,
        "a Unix stream asks where its option is known"
    );
    assert!(
        !st.inq_wanted(Kind::Udp),
        "only a stream counts what is left"
    );
    assert!(!st.inq_on(a.as_raw_fd(), Kind::Tcp));
    assert!(
        !st.inq_wanted(Kind::Tcp),
        "a refused option is not asked again"
    );
}

#[test]
fn a_tcp_socket_accepts_the_option_once() {
    let (_client, server) = tcp_pair();
    let mut st = ReadState::default();
    assert!(st.inq_on(server.as_raw_fd(), Kind::Tcp));
    assert!(st.inq_wanted(Kind::Tcp));
    assert!(st.inq_on(server.as_raw_fd(), Kind::Tcp));
}

#[test]
fn a_unix_stream_counts_what_it_leaves_where_the_kernel_can() {
    let (mut a, b) = UnixStream::pair().unwrap();
    let mut st = ReadState::default();
    if !st.inq_on(b.as_raw_fd(), Kind::Stream) {
        // A kernel before `SO_INQ` (or an architecture whose number is not known here): the
        // side reads ahead, and does not ask again.
        assert!(!st.inq_wanted(Kind::Stream));
        return;
    }
    assert!(st.inq_wanted(Kind::Stream));
    a.write_all(&[7u8; 100]).unwrap();
    let mut buf = [0u8; 64];
    let (n, left) = sys::read_inq(b.as_raw_fd(), &mut buf).unwrap();
    assert_eq!((n, left), (64, Some(36)));
    let (n, left) = sys::read_inq(b.as_raw_fd(), &mut buf).unwrap();
    assert_eq!((n, left), (36, Some(0)));
}

#[test]
fn a_unix_streams_end_is_not_counted_and_still_read() {
    let (mut a, b) = UnixStream::pair().unwrap();
    if !sys::enable_inq(b.as_raw_fd(), false) {
        return;
    }
    a.write_all(&[1u8; 64]).unwrap();
    a.shutdown(std::net::Shutdown::Write).unwrap();
    wait_for_end(b.as_raw_fd());
    let mut buf = [0u8; 64];
    // Unlike TCP's count the end is left out: the hangup's edge makes the side unsure
    // before its next read, which then finds the end.
    let (n, left) = sys::read_inq(b.as_raw_fd(), &mut buf).unwrap();
    assert_eq!((n, left), (64, Some(0)));
    let (n, _) = sys::read_inq(b.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(n, 0);
}
