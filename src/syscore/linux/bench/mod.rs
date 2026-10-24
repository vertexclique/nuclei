//! Measurements behind the shard design, as ignored tests, plus small tests in the normal
//! suite that pin the measured protocols' correctness.
//!
//! Run the measurements with
//! `cargo test -p nuclei --release --lib -- --ignored --nocapture bench_`.
//!
//! * [`wake_handshake`]: a shard parks in `epoll_wait` on an edge-triggered eventfd while
//!   foreign threads push to its inbox. Parking and waking form a Dekker pair (`parked` flag
//!   against the inbox), so each side needs a store-load barrier. Compared: `fence(SeqCst)` on
//!   both sides (A), `membarrier` on the rare parking side with a compiler fence on the waker
//!   (B), and a compiler fence only on both sides (C, the unsound control).
//! * [`fd_migration`]: one socket used from two shards, either re-registered with the shard that
//!   uses it (`EPOLL_CTL_DEL`/`ADD` under a generation token) or left on its home shard and read
//!   through a request/response mailbox.
//! * `read_path`: how a socket read reaches the caller's buffer (io_uring builds only).
//!
//! This file holds the shared plumbing (syscalls, pinning, clock, statistics, tables) and the
//! wake protocol. Measurements take [`exclusive`] so concurrently scheduled tests never share
//! pinned CPUs.
//!
//! Invariant: every cross-shard message goes through [`Wake`], so both experiments exercise the
//! same push, fence, load-`parked`, conditional-eventfd-write protocol. Atomics on the protocol
//! use `Relaxed`: on x86 a `SeqCst` store is an `xchg`, which would hide the fence being measured.

use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst},
    compiler_fence, fence,
};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use kovan::CachePadded;
use kovan_queue::seg_queue::SegQueue;

// ---------------------------------------------------------------------------------------------
// Plumbing: syscalls, pinning, clock, statistics, tables.
// ---------------------------------------------------------------------------------------------

const RUNS: usize = 5;
const LIMIT: Duration = Duration::from_secs(60);
const EFD_TOKEN: u64 = 0;
const EMPTY_EV: libc::epoll_event = libc::epoll_event { events: 0, u64: 0 };

/// Held by every measurement for its whole run: the harness runs tests concurrently, and two
/// measurements pinned to the same CPUs would corrupt each other.
fn exclusive() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn die(what: &str) -> ! {
    panic!("{what}: {}", io::Error::last_os_error())
}

fn epoll_new() -> OwnedFd {
    // SAFETY: plain syscall with a constant flag; the result is checked below.
    let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if fd < 0 {
        die("epoll_create1");
    }
    // SAFETY: `fd` is a fresh valid descriptor that nothing else owns.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn eventfd_new() -> OwnedFd {
    // SAFETY: plain syscall with constant arguments; the result is checked below.
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        die("eventfd");
    }
    // SAFETY: `fd` is a fresh valid descriptor that nothing else owns.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn epoll_ctl(ep: RawFd, op: i32, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: token };
    // SAFETY: `ev` is a valid epoll_event for the duration of the call.
    match unsafe { libc::epoll_ctl(ep, op, fd, &mut ev) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn efd_signal(fd: RawFd) {
    let one = 1u64;
    // SAFETY: writes 8 bytes from a live u64 to an eventfd.
    let n = unsafe { libc::write(fd, (&raw const one).cast(), 8) };
    assert_eq!(n, 8, "eventfd write: {}", io::Error::last_os_error());
}

fn efd_reset(fd: RawFd) {
    let mut v = 0u64;
    // SAFETY: reads at most 8 bytes into a live u64.
    let n = unsafe { libc::read(fd, (&raw mut v).cast(), 8) };
    // EAGAIN means the counter was already drained; anything else is a bug.
    assert!(
        n == 8 || io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock,
        "eventfd read: {}",
        io::Error::last_os_error()
    );
}

fn tok(e: &libc::epoll_event) -> u64 {
    e.u64
}

fn evmask(e: &libc::epoll_event) -> u32 {
    e.events
}

fn allowed_cpus() -> Vec<usize> {
    // SAFETY: a zeroed cpu_set_t is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a valid out-pointer of the size passed.
    let rc = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) };
    assert_eq!(rc, 0, "sched_getaffinity: {}", io::Error::last_os_error());
    (0..libc::CPU_SETSIZE as usize)
        // SAFETY: `c` is below CPU_SETSIZE and `set` is initialised.
        .filter(|&c| unsafe { libc::CPU_ISSET(c, &set) })
        .collect()
}

fn pin(cpu: usize) {
    // SAFETY: a zeroed cpu_set_t is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `cpu` is below CPU_SETSIZE (it came from `allowed_cpus`).
    unsafe { libc::CPU_SET(cpu, &mut set) };
    // SAFETY: pid 0 is the calling thread; `set` is initialised and the size matches.
    let rc = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) };
    assert_eq!(
        rc,
        0,
        "sched_setaffinity({cpu}): {}",
        io::Error::last_os_error()
    );
}

/// CPUs 2.. of the allowed set, so CPUs 0 and 1 stay free for the OS and the harness thread.
fn pick_cpus(n: usize) -> Vec<usize> {
    let all = allowed_cpus();
    assert!(
        all.len() >= n + 2,
        "need {} CPUs for pinning, have {}",
        n + 2,
        all.len()
    );
    all.into_iter().skip(2).take(n).collect()
}

fn involuntary_csw() -> i64 {
    // SAFETY: a zeroed rusage is valid; getrusage fills it.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid out-pointer.
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) };
    // A `c_long`: 32 bits wide on 32-bit targets.
    i64::from(ru.ru_nivcsw)
}

#[derive(Clone, Copy)]
struct Clock(Instant);

impl Clock {
    fn new() -> Self {
        Self(Instant::now())
    }
    fn ns(self) -> u64 {
        self.0.elapsed().as_nanos() as u64
    }
}

/// Busy-wait for `ns` (deadline based, no sleeping).
fn spin_for(clock: Clock, ns: u64) {
    let end = clock.ns() + ns;
    while clock.ns() < end {
        std::hint::spin_loop();
    }
}

/// Bounded spin, then yield, until `cond` holds; fails loudly past `LIMIT`.
fn spin_until(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    let mut i = 0u32;
    while !cond() {
        i += 1;
        if i < 2048 {
            std::hint::spin_loop();
        } else {
            thread::yield_now();
            assert!(start.elapsed() < LIMIT, "stalled waiting for {what}");
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Quantile of a sorted slice (nearest rank); NaN when empty.
fn quant(sorted: &[u32], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let rank = (sorted.len() as f64 * q).ceil() as usize;
    f64::from(sorted[rank.clamp(1, sorted.len()) - 1])
}

fn sorted(mut v: Vec<u32>) -> Vec<u32> {
    v.sort_unstable();
    v
}

type Row = (String, Vec<f64>);

/// Column-wise pick (`v` is sorted ascending, NaN already dropped) over runs; NaN when empty.
fn reduce_cols(runs: &[Vec<f64>], pick: fn(&[f64]) -> f64) -> Vec<f64> {
    (0..runs[0].len())
        .map(|c| {
            let mut v: Vec<f64> = runs.iter().map(|r| r[c]).filter(|x| !x.is_nan()).collect();
            v.sort_by(f64::total_cmp);
            if v.is_empty() { f64::NAN } else { pick(&v) }
        })
        .collect()
}

fn median_cols(runs: &[Vec<f64>]) -> Vec<f64> {
    reduce_cols(runs, |v| v[v.len() / 2])
}

/// The spread of a metric over runs, as two extra rows (`min` and `max`) beside the median.
fn spread_rows(label: &str, runs: &[Vec<f64>]) -> [Row; 2] {
    [
        (format!("{label} [min]"), reduce_cols(runs, |v| v[0])),
        (
            format!("{label} [max]"),
            reduce_cols(runs, |v| v[v.len() - 1]),
        ),
    ]
}

fn num(x: f64) -> String {
    if x.is_nan() {
        "-".into()
    } else if x.abs() >= 100.0 || x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x:.3}")
    }
}

fn table(title: &str, cols: &[(&str, usize)], rows: &[Row]) {
    println!("\n{title}");
    print!("{:<36}", "variant");
    for (h, _) in cols {
        print!(" {h:>w$}", w = h.len().max(9));
    }
    println!();
    for (label, v) in rows {
        print!("{label:<36}");
        for (h, c) in cols {
            print!(" {:>w$}", num(v[*c]), w = h.len().max(9));
        }
        println!();
    }
}

fn banner(name: &str, cpus: &[usize]) {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let cpuinfo = read("/proc/cpuinfo");
    let model = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split_once(':'))
        .map_or("unknown", |(_, v)| v.trim());
    println!("\n== {name} ==");
    println!("cpu model : {model}");
    println!(
        "nproc     : {}",
        thread::available_parallelism().map_or(0, |n| n.get())
    );
    println!("kernel    : {}", read("/proc/sys/kernel/osrelease").trim());
    println!("cpus used : {cpus:?}");
    println!("loadavg   : {}", read("/proc/loadavg").trim());
}

/// Median cost of one `Clock::ns` call pair, the floor under every timed region below.
fn timer_floor(clock: Clock) -> f64 {
    let d: Vec<u32> = (0..100_000)
        .map(|_| {
            let t0 = clock.ns();
            (clock.ns() - t0) as u32
        })
        .collect();
    quant(&sorted(d), 0.5)
}

// ---------------------------------------------------------------------------------------------
// The wake protocol shared by both experiments.
// ---------------------------------------------------------------------------------------------

/// A store-load barrier policy for the park/wake Dekker pair.
trait Protocol {
    const NAME: &'static str;
    /// One-time setup (membarrier registration).
    fn prepare() {}
    /// Run by the shard between publishing `parked` and re-checking the inbox.
    fn shard_fence();
    /// Run by a waker between the inbox push and the `parked` load.
    fn waker_fence();
}

struct SeqCstBoth;
struct Membarrier;
struct NoFence;

impl Protocol for SeqCstBoth {
    const NAME: &'static str = "A fence(SeqCst) both";
    fn shard_fence() {
        fence(SeqCst);
    }
    fn waker_fence() {
        fence(SeqCst);
    }
}

impl Protocol for Membarrier {
    const NAME: &'static str = "B membarrier shard";
    fn prepare() {
        if let Err(e) = membarrier_register() {
            panic!("membarrier private-expedited registration failed: {e}");
        }
    }
    fn shard_fence() {
        membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED).expect("membarrier(PRIVATE_EXPEDITED)");
    }
    fn waker_fence() {
        compiler_fence(SeqCst);
    }
}

// The control keeps the compiler fence so a lost wakeup can only be hardware reordering.
impl Protocol for NoFence {
    const NAME: &'static str = "C no hw fence (control)";
    fn shard_fence() {
        compiler_fence(SeqCst);
    }
    fn waker_fence() {
        compiler_fence(SeqCst);
    }
}

const MEMBARRIER_CMD_PRIVATE_EXPEDITED: libc::c_int = 1 << 3;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: libc::c_int = 1 << 4;

fn membarrier(cmd: libc::c_int) -> io::Result<()> {
    // SAFETY: membarrier takes only integer arguments.
    match unsafe { libc::syscall(libc::SYS_membarrier, cmd, 0, 0) } {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

/// Registers the process once; `Ok` carries the registration cost of the first call in ns.
fn membarrier_register() -> &'static Result<u64, String> {
    static REG: OnceLock<Result<u64, String>> = OnceLock::new();
    REG.get_or_init(|| {
        let t = Instant::now();
        membarrier(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED)
            .map(|()| t.elapsed().as_nanos() as u64)
            .map_err(|e| e.to_string())
    })
}

/// A shard's inbox plus the machinery to park it: `parked`, an eventfd, and an epoll set holding
/// the eventfd edge-triggered (the migration measurement adds the socket to the same set).
struct Wake<M> {
    inbox: SegQueue<M>,
    parked: CachePadded<AtomicBool>,
    efd: OwnedFd,
    ep: OwnedFd,
}

impl<M: Send + 'static> Wake<M> {
    fn new() -> Self {
        let (efd, ep) = (eventfd_new(), epoll_new());
        let mask = (libc::EPOLLIN | libc::EPOLLET) as u32;
        epoll_ctl(
            ep.as_raw_fd(),
            libc::EPOLL_CTL_ADD,
            efd.as_raw_fd(),
            mask,
            EFD_TOKEN,
        )
        .expect("register eventfd");
        Self {
            inbox: SegQueue::new(),
            parked: CachePadded::new(AtomicBool::new(false)),
            efd,
            ep,
        }
    }

    /// Waker side. True when the shard was seen parked and the eventfd was written.
    fn send<P: Protocol>(&self, msg: M) -> bool {
        self.inbox.push(msg);
        P::waker_fence();
        let parked = self.parked.load(Relaxed);
        if parked {
            efd_signal(self.efd.as_raw_fd());
        }
        parked
    }

    /// Shard side, first half of parking: publish `parked`, fence, re-check the inbox.
    /// True when the inbox is empty and sleeping is safe.
    fn arm<P: Protocol>(&self) -> bool {
        self.parked.store(true, Relaxed);
        P::shard_fence();
        let empty = self.inbox.is_empty();
        if !empty {
            self.parked.store(false, Relaxed);
        }
        empty
    }

    /// Shard side, second half: block in `epoll_wait`, clear `parked`, reset the eventfd if it
    /// fired. Returns the event count (0 means the timeout expired).
    fn wait(&self, ev: &mut [libc::epoll_event], timeout_ms: i32) -> usize {
        let n = loop {
            // SAFETY: `ev` is a valid buffer of `ev.len()` events.
            let n = unsafe {
                libc::epoll_wait(
                    self.ep.as_raw_fd(),
                    ev.as_mut_ptr(),
                    ev.len() as i32,
                    timeout_ms,
                )
            };
            if n >= 0 {
                break n as usize;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                die("epoll_wait");
            }
        };
        self.parked.store(false, Relaxed);
        if ev[..n].iter().any(|e| tok(e) == EFD_TOKEN) {
            efd_reset(self.efd.as_raw_fd());
        }
        n
    }
}

mod fd_migration;
#[cfg(feature = "iouring")]
mod read_path;
mod wake_handshake;
