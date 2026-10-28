//! How a socket read reaches the caller's buffer, measured: the provided-buffer pool against
//! readiness followed by a direct read.
//!
//! Three read paths are compared over one loopback TCP connection fed by a writer thread:
//!
//! * A: multishot `recv` into a provided-buffer pool, a copy into the caller's buffer, and an
//!   immediate recycle of the pool buffer (a read completes when the caller's buffer is full or
//!   the pooled data is exhausted, like a `poll_read`);
//! * B: an io_uring multishot poll, then `recv` straight into the caller's buffer until `EAGAIN`;
//! * C: edge-triggered epoll, then the same direct `recv`.
//!
//! The correctness tests run by default. The measurement is `#[ignore]`d, test-only and prints one
//! table per workload plus the size threshold its rule yields; run it in a release build with
//! `--ignored --nocapture`. Invariants: in the measurement the reader and the writer are pinned
//! to distinct CPUs, a measured run fails loudly instead of returning a short count, and nothing
//! here is reachable from product code.
//!
//! The file is split along its seams to stay under the size limit: `readers` holds the three
//! read paths, `workloads` the writer, the destination buffers and the two workloads, `report`
//! the aggregation over runs, the tables and the threshold rule; this file holds the shared
//! constants, the syscall helpers and the tests.

use std::{
    collections::{BTreeMap, VecDeque},
    env, fs,
    io::{self, ErrorKind, Read, Write},
    mem::{size_of, zeroed},
    net::{Shutdown, TcpListener, TcpStream},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    process::Command,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use super::super::iouring::ring::{IORING_CQE_F_MORE, Ring, RingConfig, Sqe, buf_ring::BufRing};

mod readers;
mod report;
mod workloads;

use readers::*;
use report::*;
use workloads::*;

/// Streaming read sizes.
const SIZES: [usize; 5] = [512, 4 << 10, 16 << 10, 64 << 10, 256 << 10];
/// Ping-pong sizes: the three asked for, plus `512 B`, `4 KiB` and `256 KiB` so the threshold rule
/// has a latency figure at every streaming size.
const PP_SIZES: [usize; 6] = [512, 1 << 10, 4 << 10, 16 << 10, 64 << 10, 256 << 10];
const RUNS: usize = 5;
const MAX_ATTEMPTS: usize = 3;
/// A point whose throughput (or latency) range exceeds this fraction of its median is rerun.
const SPREAD_LIMIT: f64 = 0.15;
const STREAM_BYTES: u64 = 2 << 30;
const STREAM_SECS: f64 = 5.0;
const PP_ROUND_TRIPS: usize = 100_000;
const PP_MAX_SECS: f64 = 8.0;
const PP_WARMUP: usize = 1_000;
const WRITE_CHUNK: usize = 64 << 10;
/// A prime-ish length that is not a power of two, so a reordered or wrapped stream cannot match.
const PATTERN_LEN: usize = (1 << 20) + 13;
const WIN: f64 = 1.05;
const RECV_UD: u64 = 1;
const CANCEL_UD: u64 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Path {
    Pool,
    UringPoll,
    Epoll,
}

impl Path {
    const ALL: [Path; 3] = [Path::Pool, Path::UringPoll, Path::Epoll];

    fn tag(self) -> &'static str {
        match self {
            Path::Pool => "A",
            Path::UringPoll => "B",
            Path::Epoll => "C",
        }
    }
}

/// Simple per-run counters, bumped in the loops themselves.
#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    reads: u64,
    bytes: u64,
    enter: u64,
    recv: u64,
    epoll_wait: u64,
    send: u64,
    enobufs: u64,
}

#[derive(Clone, Copy, Debug)]
struct Cpus {
    reader: usize,
    writer: usize,
}

/// Reader and writer CPUs: `NUCLEI_BENCH_CPUS=reader,writer`, else 2 and 3.
fn cpus() -> Cpus {
    env::var("NUCLEI_BENCH_CPUS")
        .ok()
        .and_then(|v| {
            let (r, w) = v.split_once(',')?;
            Some(Cpus {
                reader: r.trim().parse().ok()?,
                writer: w.trim().parse().ok()?,
            })
        })
        .unwrap_or(Cpus {
            reader: 2,
            writer: 3,
        })
}

fn cvt(r: i32) -> io::Result<i32> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn pin(cpu: usize) -> io::Result<()> {
    // SAFETY: `set` is a zeroed cpu_set_t that the libc macros fill; pid 0 is the calling thread.
    unsafe {
        let mut set: libc::cpu_set_t = zeroed();
        libc::CPU_SET(cpu, &mut set);
        cvt(libc::sched_setaffinity(
            0,
            size_of::<libc::cpu_set_t>(),
            &set,
        ))
        .map(drop)
    }
}

/// CPU time (user plus system) charged to the calling thread so far, in nanoseconds.
fn thread_cpu_ns() -> u64 {
    // SAFETY: `ru` is a zeroed rusage that the call fills.
    let ru = unsafe {
        let mut ru: libc::rusage = zeroed();
        libc::getrusage(libc::RUSAGE_THREAD, &mut ru);
        ru
    };
    let ns = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1_000;
    ns(ru.ru_utime) + ns(ru.ru_stime)
}

fn loadavg() -> String {
    fs::read_to_string("/proc/loadavg").map_or_else(|_| "unavailable".into(), |s| s.trim().into())
}

#[test]
#[ignore = "measurement: run in release with --ignored --nocapture on a quiet machine"]
fn bench_read_path() {
    if !uring_available() {
        return;
    }
    let cpus = cpus();
    let (l3, src) = l3_bytes();
    println!("load average before: {}", loadavg());
    println!(
        "reader cpu {}, writer cpu {} (sched_setaffinity, distinct)",
        cpus.reader, cpus.writer
    );
    println!(
        "L3 {} from {src}; the cold destination rotates through {}",
        human(l3),
        human(4 * l3)
    );
    println!(
        "{RUNS} runs per point; streaming ends at {} or {STREAM_SECS} s; a point whose range exceeds {:.0}% of its median is rerun up to {MAX_ATTEMPTS} times",
        human(STREAM_BYTES as usize),
        SPREAD_LIMIT * 100.0
    );
    let sized = |s| (s, size_pool(s, cpus, 4 * l3).expect("pool sizing failed"));
    let pools: Pools = PP_SIZES.map(sized).into();
    println!(
        "\n== path A pool per read size (count = 4x the smallest count that never ran dry in a hot and a cold probe) =="
    );
    let cells: Vec<_> = pools
        .iter()
        .map(|(&s, c)| {
            let bytes = c.buf_size as usize * usize::from(c.count);
            let buf = human(c.buf_size as usize);
            vec![human(s), buf, c.count.to_string(), human(bytes)]
        })
        .collect();
    table(&["S", "buffer", "count", "pool"], &cells);
    let hot = measure_stream("hot", None, cpus, &pools).expect("hot streaming failed");
    let cold = measure_stream("cold", Some(4 * l3), cpus, &pools).expect("cold streaming failed");
    let pp = measure_pingpong(cpus, &pools).expect("ping-pong failed");
    verdict(&hot, &cold, &pp);
    println!("\nload average after: {}", loadavg());
}

/// Reads `total` stream bytes through `path` in reads of `s` bytes and checks them in order.
fn check_stream(path: Path, s: usize, total: u64, cfg: PoolCfg) {
    let (conn, peer) = connected_pair().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = thread::spawn(move || feed(peer, total, &stop));
    let mut r = Reader::new(path, conn.as_raw_fd(), cfg).unwrap();
    let (mut buf, mut cur, mut got) = (vec![0u8; s], Cursor::default(), 0u64);
    loop {
        let n = r.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        cur.verify(&buf[..n]);
        got += n as u64;
    }
    assert_eq!(
        got, total,
        "{path:?} S={s}: bytes received differ from bytes sent"
    );
    drop(r);
    writer.join().unwrap().unwrap();
}

fn correctness(path: Path) {
    if path != Path::Epoll && !uring_available() {
        return;
    }
    let pool = |count| PoolCfg {
        buf_size: 4096,
        count,
    };
    // Reads below, at, above and across the pool buffer size, and a pool of four buffers so the
    // receive runs dry and is re-armed.
    let cases = [(1000, 64), (4096, 64), (10_000, 64), (10_000, 4), (512, 4)];
    for (s, count) in cases {
        check_stream(path, s, 3 << 20, pool(count));
    }
    for s in [1 << 10, 20_000] {
        pingpong_run(path, s, 200, pool(64), None, true).unwrap();
    }
}

#[test]
fn read_path_pool_is_exact() {
    correctness(Path::Pool);
}

#[test]
fn read_path_uring_poll_is_exact() {
    correctness(Path::UringPoll);
}

#[test]
fn read_path_epoll_is_exact() {
    correctness(Path::Epoll);
}
