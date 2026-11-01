//! A listener whose process runs out of descriptors: the accept says so, as
//! often as it is retried, and the connections that waited in the backlog are
//! served once descriptors are free again, with nothing spinning in between.
//! Each case runs alone in its process, for it lowers the descriptor limit.

use std::fs::File;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use super::{next, runtime, within};
use crate::alone::{alone, open_descriptors};
use crate::io::Handle;

const CLIENTS: usize = 4;

/// Accepts tried while no descriptor is free.
const RETRIES: usize = 3;

/// The connections of the backlog the first accept to find no descriptor
/// closes: on macOS it takes one and drops it (XNU's `accept_nocancel`,
/// bsd/kern/uipc_syscalls.c), elsewhere it leaves the backlog whole. The
/// retries after it take none.
const CLOSED: usize = if cfg!(target_vendor = "apple") { 1 } else { 0 };

/// How many of `clients` the listener's side closed: a closed one reads the
/// end or a reset, a served one has nothing to read yet.
fn closed(clients: &[impl AsRawFd]) -> usize {
    clients
        .iter()
        .filter(|client| {
            let mut byte = 0u8;
            // SAFETY: a peek of at most one byte into our own byte, on a
            // descriptor the caller keeps open.
            let n = unsafe {
                libc::recv(
                    client.as_raw_fd(),
                    (&raw mut byte).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            n == 0 || (n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock)
        })
        .count()
}

/// Lowers the limit to what is open plus a few and uses those up; dropping
/// the guard gives the descriptors back.
struct Starved {
    held: Vec<File>,
}

impl Starved {
    fn new() -> Self {
        // A limit is a descriptor number: the room is above the highest open.
        let room = open_descriptors().into_iter().max().unwrap_or(0) as libc::rlim_t + 8;
        let limit = libc::rlimit {
            rlim_cur: room,
            rlim_max: room,
        };
        // SAFETY: lowers this process's own limit with a valid struct.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut held = Vec::new();
        while let Ok(null) = File::open("/dev/null") {
            held.push(null);
        }
        Starved { held }
    }

    fn free(&mut self) {
        self.held.clear();
    }
}

fn cpu() -> Duration {
    // SAFETY: plain data, all zeroes valid; the kernel fills it.
    let mut ts: libc::timespec = unsafe { core::mem::zeroed() };
    // SAFETY: a plain call with a valid struct.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) },
        0
    );
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn is_out_of_descriptors(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

/// Idle for a while and checks the process burned a small part of it.
async fn idles() {
    let (start, spent) = (cpu(), Duration::from_millis(300));
    crate::time::sleep(spent).await;
    let burned = cpu() - start;
    assert!(burned < spent / 3, "{burned:?} of cpu while {spent:?} idle");
}

fn tcp_case(multi: bool) {
    runtime().block_on(async {
        let listener = Handle::<TcpListener>::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let mut stream = within(listener.accept_multi()).await.unwrap();
        let clients: Vec<_> = (0..CLIENTS)
            .map(|_| TcpStream::connect(addr).unwrap())
            .collect();
        let mut starved = Starved::new();
        let mut served = Vec::new();
        if multi {
            let first = crate::time::timeout(Duration::from_millis(300), next(&mut stream)).await;
            assert!(
                first.is_err(),
                "no descriptor left, yet the stream moved: {:?}, {:?}",
                first.as_ref().map(|c| c.is_some()),
                stream.take_error()
            );
            starved.free();
            for _ in 0..CLIENTS - CLOSED {
                served.push(within(next(&mut stream)).await.expect("the stream ended"));
            }
            assert!(stream.take_error().is_none());
        } else {
            for _ in 0..RETRIES {
                let err = within(listener.accept()).await.unwrap_err();
                assert!(is_out_of_descriptors(&err), "{err}");
            }
            idles().await;
            starved.free();
            for _ in 0..CLIENTS - CLOSED {
                served.push(within(listener.accept()).await.unwrap().0);
            }
        }
        assert_eq!(closed(&clients), CLOSED);
    });
}

fn unix_case() {
    let path = std::env::temp_dir().join(format!("nuclei-emfile-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    runtime().block_on(async {
        let listener = Handle::<UnixListener>::bind(&path).unwrap();
        let clients: Vec<_> = (0..CLIENTS)
            .map(|_| UnixStream::connect(&path).unwrap())
            .collect();
        let mut starved = Starved::new();
        for _ in 0..RETRIES {
            let err = within(listener.accept()).await.unwrap_err();
            assert!(is_out_of_descriptors(&err), "{err}");
        }
        idles().await;
        starved.free();
        let mut served = Vec::new();
        for _ in 0..CLIENTS - CLOSED {
            served.push(within(listener.accept()).await.unwrap().0);
        }
        assert_eq!(closed(&clients), CLOSED);
    });
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_tcp_accept_out_of_descriptors_fails_and_later_serves_the_backlog() {
    if alone(
        "net::tests::emfile::a_tcp_accept_out_of_descriptors_fails_and_later_serves_the_backlog",
    ) {
        tcp_case(false);
    }
}

#[test]
fn a_tcp_accept_multi_out_of_descriptors_waits_for_descriptors_and_serves_the_backlog() {
    if alone(
        "net::tests::emfile::a_tcp_accept_multi_out_of_descriptors_waits_for_descriptors_and_serves_the_backlog",
    ) {
        tcp_case(true);
    }
}

#[test]
fn a_unix_accept_out_of_descriptors_fails_and_later_serves_the_backlog() {
    if alone(
        "net::tests::emfile::a_unix_accept_out_of_descriptors_fails_and_later_serves_the_backlog",
    ) {
        unix_case();
    }
}
