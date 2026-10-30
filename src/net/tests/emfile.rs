//! A listener whose process runs out of descriptors: the accept says so, and
//! the connections that waited in the backlog are served once descriptors are
//! free again, with nothing spinning in between. Each case runs alone in its
//! process, for it lowers the descriptor limit.

use std::fs::File;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use super::{next, runtime, within};
use crate::alone::alone;
use crate::io::Handle;

const CLIENTS: usize = 4;

fn open_descriptors() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, Iterator::count)
}

/// Lowers the limit to what is open plus a few and uses those up; dropping
/// the guard gives the descriptors back.
struct Starved {
    held: Vec<File>,
}

impl Starved {
    fn new() -> Self {
        let limit = libc::rlimit {
            rlim_cur: (open_descriptors() + 8) as libc::rlim_t,
            rlim_max: (open_descriptors() + 8) as libc::rlim_t,
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
        if multi {
            let first = crate::time::timeout(Duration::from_millis(300), next(&mut stream)).await;
            assert!(first.is_err(), "no descriptor left, yet the stream moved");
            starved.free();
            for _ in 0..CLIENTS {
                within(next(&mut stream)).await.expect("the stream ended");
            }
            assert!(stream.take_error().is_none());
            return;
        }
        let err = within(listener.accept()).await.unwrap_err();
        assert!(is_out_of_descriptors(&err), "{err}");
        idles().await;
        starved.free();
        for _ in 0..CLIENTS {
            within(listener.accept()).await.unwrap();
        }
        drop(clients);
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
        let err = within(listener.accept()).await.unwrap_err();
        assert!(is_out_of_descriptors(&err), "{err}");
        idles().await;
        starved.free();
        for _ in 0..CLIENTS {
            within(listener.accept()).await.unwrap();
        }
        drop(clients);
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
