//! `TCP_INQ`: what a read of a TCP socket leaves is counted by the kernel, so
//! a read that filled its buffer still knows whether the socket is empty, and
//! a kernel that refuses the option leaves the side on the read-ahead path.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;

use super::*;
use crate::io::source::read::ReadState;

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
    assert!(sys::enable_inq(server.as_raw_fd()), "TCP_INQ is refused");
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
    assert!(sys::enable_inq(server.as_raw_fd()), "TCP_INQ is refused");
    client.write_all(&[1u8; 64]).unwrap();
    let mut buf = [0u8; 64];
    let (n, left) = sys::read_inq(server.as_raw_fd(), &mut buf).unwrap();
    assert_eq!((n, left), (64, Some(0)));
}

#[test]
fn a_pending_end_of_stream_is_not_an_empty_socket() {
    let (mut client, server) = tcp_pair();
    assert!(sys::enable_inq(server.as_raw_fd()), "TCP_INQ is refused");
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

#[test]
fn a_socket_that_refuses_the_option_falls_back() {
    // A Unix socket refuses `TCP_INQ` as the TCP socket of an old kernel does.
    let (a, _b) = UnixStream::pair().unwrap();
    let mut st = ReadState::default();
    assert!(st.inq_wanted(Kind::Tcp), "the option is tried first");
    assert!(!st.inq_wanted(Kind::Stream), "only TCP counts what is left");
    assert!(!st.inq_on(a.as_raw_fd()));
    assert!(
        !st.inq_wanted(Kind::Tcp),
        "a refused option is not asked again"
    );
}

#[test]
fn a_tcp_socket_accepts_the_option_once() {
    let (_client, server) = tcp_pair();
    let mut st = ReadState::default();
    assert!(st.inq_on(server.as_raw_fd()));
    assert!(st.inq_wanted(Kind::Tcp));
    assert!(st.inq_on(server.as_raw_fd()));
}
