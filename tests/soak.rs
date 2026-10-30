//! The soak: every kind of work at once for a long time, with faults among
//! it, checking every byte and watching what the process holds.
//!
//! Short connections come and go (some reset by their client, some given up
//! part way, some read through timeouts that fire); long-lived streams carry
//! bytes both ways with owned and plain calls; datagrams go back and forth
//! over UDP and Unix sockets; files are written, synced, read back and
//! removed; timers fire and timeouts lapse; the blocking pool works. Every
//! byte that arrives is checked against what was sent. Every few seconds the
//! process's resident memory, live heap bytes, open descriptors and threads
//! are sampled; after a warm-up none may rise past a small slack, and the
//! run fails with the whole series if one does.
//!
//! It is long, so it is ignored by default:
//! `NUCLEI_SOAK_SECS=3600 cargo test --release --test soak -- --ignored --nocapture`
//! (the length defaults to an hour; `NUCLEI_BACKEND` picks the backend).

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream, UdpSocket};

struct Live;

static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: every call goes to the system allocator as it came.
unsafe impl GlobalAlloc for Live {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as above.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as above.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_add(
                new_size as isize - layout.size() as isize,
                Ordering::Relaxed,
            );
        }
        new
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Live = Live;

/// How long one step of any workload may take before the soak fails as hung.
const DEADLINE: Duration = Duration::from_secs(120);
/// How often what the process holds is sampled.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);

/// The byte at `offset` of stream `seed`: both ends compute it, so every
/// byte is checked where it lands.
fn byte_at(seed: u64, offset: u64) -> u8 {
    let mut x = (seed ^ offset.wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_add(offset);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 29;
    x as u8
}

fn fill(seed: u64, from: u64, buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = byte_at(seed, from + i as u64);
    }
}

fn check(what: &str, seed: u64, from: u64, got: &[u8]) {
    if let Some(i) = (0..got.len()).find(|&i| got[i] != byte_at(seed, from + i as u64)) {
        panic!("{what}: byte {} of stream {seed} is wrong", from + i as u64);
    }
}

/// A small generator of the soak's choices, one per task.
struct Dice(u64);

impl Dice {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

async fn within<F: core::future::Future>(what: &str, fut: F) -> F::Output {
    match nuclei::time::timeout(DEADLINE, fut).await {
        Ok(out) => out,
        Err(_) => panic!("soak: {what} had no answer in {DEADLINE:?}"),
    }
}

/// What the soak has checked and done, and when to stop.
#[derive(Default)]
struct Totals {
    stop: AtomicBool,
    /// The server of the short connections stops once its clients have.
    stop_serving: AtomicBool,
    bytes: AtomicU64,
    connections: AtomicU64,
    resets: AtomicU64,
    cancelled: AtomicU64,
    datagrams: AtomicU64,
    files: AtomicU64,
    timers: AtomicU64,
    blocking: AtomicU64,
}

impl Totals {
    fn checked(&self, n: usize) {
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
    }

    fn going(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }
}

/// The server of the short connections: each request is a length and that
/// many bytes of the stream its seed names; the answer echoes them.
async fn serve(listener: TcpListener, totals: Arc<Totals>) {
    let mut incoming = listener.accept_multi().await.unwrap();
    while !totals.stop_serving.load(Ordering::Relaxed) {
        let next = core::future::poll_fn(|cx| {
            futures_core::Stream::poll_next(core::pin::Pin::new(&mut incoming), cx)
        });
        let Ok(conn) = nuclei::time::timeout(Duration::from_millis(200), next).await else {
            continue;
        };
        let Some(mut conn) = conn else {
            panic!("soak: the accept stream ended: {:?}", incoming.take_error());
        };
        nuclei::spawn(async move {
            let mut head = [0u8; 16];
            if conn.read_exact(&mut head).await.is_err() {
                return;
            }
            let seed = u64::from_le_bytes(head[..8].try_into().unwrap());
            let len = u64::from_le_bytes(head[8..].try_into().unwrap()) as usize;
            let mut body = vec![0u8; len];
            if conn.read_exact(&mut body).await.is_err() {
                return;
            }
            check("a request", seed, 0, &body);
            let _ = conn.write_owned(body).await;
            let _ = conn.shutdown().await;
        })
        .detach();
    }
}

/// Short connections, one after another: most complete and check the echo;
/// some reset part way, some give up part way, some read through timeouts.
async fn churn(addr: std::net::SocketAddr, mut dice: Dice, totals: Arc<Totals>) {
    while totals.going() {
        let seed = dice.next();
        let len = 1 + dice.below(256 * 1024) as usize;
        let mut conn = within("a connect", TcpStream::connect(addr)).await.unwrap();
        let mut request = vec![0u8; 16 + len];
        request[..8].copy_from_slice(&seed.to_le_bytes());
        request[8..16].copy_from_slice(&(len as u64).to_le_bytes());
        fill(seed, 0, &mut request[16..]);
        match dice.below(10) {
            0 => {
                // Reset after part of the request.
                let cut = 16 + dice.below(len as u64) as usize;
                let _ = conn.write_all(&request[..cut]).await;
                let _ = conn.flush().await;
                conn.set_zero_linger().unwrap();
                drop(conn);
                totals.resets.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            1 => {
                // Gives up on the owned write part way: whole or none of it goes.
                let tries = Duration::from_micros(dice.below(500));
                let _ = nuclei::time::timeout(tries, conn.write_owned(request)).await;
                totals.cancelled.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            _ => {}
        }
        within("a request", async {
            if dice.below(2) == 0 {
                let (wrote, _) = conn.write_owned(request).await;
                wrote.map(drop)
            } else {
                conn.write_all(&request).await?;
                conn.flush().await
            }
        })
        .await
        .unwrap();
        // The answer, read through timeouts that may fire: a read given up
        // loses nothing, so the bytes still come in order.
        let mut got = 0usize;
        let mut buf = vec![0u8; 64 * 1024];
        let started = Instant::now();
        while got < len {
            assert!(started.elapsed() < DEADLINE, "soak: an answer hung");
            let patience = Duration::from_micros(50 + dice.below(2000));
            match nuclei::time::timeout(patience, conn.read(&mut buf)).await {
                Err(_) => {
                    totals.cancelled.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Ok(0)) => panic!("soak: the answer ended after {got} of {len} bytes"),
                Ok(Ok(n)) => {
                    check("an answer", seed, got as u64, &buf[..n]);
                    got += n;
                }
                Ok(Err(err)) => panic!("soak: an answer failed: {err}"),
            }
        }
        totals.checked(len * 2);
        totals.connections.fetch_add(1, Ordering::Relaxed);
    }
}

/// A long-lived stream, carrying bytes one way with owned and plain writes
/// in pieces of every size, read with plain and owned reads.
async fn stream(mut dice: Dice, totals: Arc<Totals>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (conn, accepted) = nuclei::join!(TcpStream::connect(addr), listener.accept());
    let (mut tx, rx) = (conn.unwrap(), accepted.unwrap().0);
    let seed = dice.next();
    let reader = {
        let totals = Arc::clone(&totals);
        nuclei::spawn(async move {
            let mut at = 0u64;
            let mut rx = rx;
            let mut owned = false;
            loop {
                owned = !owned;
                let (n, bytes) = if owned {
                    let (n, buf) = within(
                        "a stream read",
                        rx.read_owned(Vec::with_capacity(48 * 1024)),
                    )
                    .await;
                    (n.unwrap(), buf)
                } else {
                    let mut buf = vec![0u8; 20_000];
                    let n = within("a stream read", rx.read(&mut buf)).await.unwrap();
                    buf.truncate(n);
                    (n, buf)
                };
                if n == 0 {
                    return at;
                }
                check("a stream", seed, at, &bytes);
                at += n as u64;
                totals.checked(n);
            }
        })
    };
    let mut at = 0u64;
    while totals.going() {
        let order = dice.below(21);
        let len = 1 + dice.below(1 << order) as usize;
        let mut piece = vec![0u8; len];
        fill(seed, at, &mut piece);
        if dice.below(2) == 0 {
            let (wrote, _) = within("a stream write", tx.write_owned(piece)).await;
            wrote.unwrap();
        } else {
            within("a stream write", tx.write_all(&piece))
                .await
                .unwrap();
        }
        at += len as u64;
    }
    within("a stream shutdown", tx.shutdown()).await.unwrap();
    let read = within("a stream's reader", reader).await;
    assert_eq!(read, at, "soak: a stream's reader read another count");
}

/// Datagrams over UDP, each checked whole by the number and length it
/// carries. A receive given up on loses nothing: the datagram it would have
/// taken comes to the next one, so a receive may get an earlier datagram,
/// never one twice and never out of order on loopback.
async fn datagrams(mut dice: Dice, totals: Arc<Totals>) {
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let to = b.local_addr().unwrap();
    let mut next = 0u64;
    let mut last: Option<u64> = None;
    while totals.going() {
        let seq = next;
        next += 1;
        let len = 16 + dice.below(60_000) as usize;
        let mut bytes = vec![0u8; len];
        bytes[..8].copy_from_slice(&seq.to_le_bytes());
        bytes[8..16].copy_from_slice(&(len as u64).to_le_bytes());
        fill(seq, 16, &mut bytes[16..]);
        let sent = if dice.below(2) == 0 {
            a.send_to_owned(bytes, to).await.0
        } else {
            a.send_to(&bytes, to).await
        };
        assert_eq!(sent.unwrap(), len);
        let patience = Duration::from_millis(500);
        let got =
            nuclei::time::timeout(patience, b.recv_from_owned(Vec::with_capacity(65_536))).await;
        match got {
            // The kernel may drop a datagram, and a receive may be given up on.
            Err(_) => totals.cancelled.fetch_add(1, Ordering::Relaxed),
            Ok((n, buf)) => {
                let (n, _) = n.unwrap();
                assert!(n >= 16, "soak: a datagram of {n} bytes");
                let seq = u64::from_le_bytes(buf[..8].try_into().unwrap());
                let want = u64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize;
                assert_eq!(n, want, "soak: datagram {seq} cut");
                assert!(
                    last.is_none_or(|l| seq > l),
                    "soak: datagram {seq} after {last:?}"
                );
                last = Some(seq);
                check("a datagram", seq, 16, &buf[16..]);
                totals.checked(n);
                totals.datagrams.fetch_add(1, Ordering::Relaxed)
            }
        };
    }
}

/// Datagrams over a Unix socket, which loses none: each arrives, in order.
#[cfg(unix)]
async fn unix_datagrams(mut dice: Dice, totals: Arc<Totals>) {
    let (a, b) = nuclei::net::UnixDatagram::pair().unwrap();
    let mut seq = 0u64;
    while totals.going() {
        let len = 1 + dice.below(4000) as usize;
        let mut bytes = vec![0u8; len];
        fill(seq, 0, &mut bytes);
        let (sent, _) = within("a Unix send", a.send_owned(bytes)).await;
        assert_eq!(sent.unwrap(), len);
        let mut buf = vec![0u8; 8192];
        let n = within("a Unix receive", b.recv(&mut buf)).await.unwrap();
        assert_eq!(n, len, "soak: a Unix datagram cut or lost");
        check("a Unix datagram", seq, 0, &buf[..n]);
        totals.checked(n);
        totals.datagrams.fetch_add(1, Ordering::Relaxed);
        seq += 1;
    }
}

/// Files written in pieces, synced, read back whole and removed.
async fn files(dir: std::path::PathBuf, mut dice: Dice, totals: Arc<Totals>) {
    let mut n = 0u64;
    let me = dice.0;
    while totals.going() {
        let path = dir.join(format!("soak-{me}-{n}"));
        n += 1;
        let seed = dice.next();
        let len = dice.below(2 << 20) as usize;
        let mut bytes = vec![0u8; len];
        fill(seed, 0, &mut bytes);
        let mut file = within("a file create", nuclei::fs::File::create(&path))
            .await
            .unwrap();
        let mut at = 0;
        while at < len {
            let step = (1 + dice.below(300_000) as usize).min(len - at);
            if dice.below(2) == 0 {
                let (wrote, _) = within(
                    "a file write",
                    file.write_owned(bytes[at..at + step].to_vec()),
                )
                .await;
                wrote.unwrap();
            } else {
                within("a file write", file.write_all(&bytes[at..at + step]))
                    .await
                    .unwrap();
            }
            at += step;
        }
        within("a file flush", file.flush()).await.unwrap();
        within("a file sync", file.sync_all()).await.unwrap();
        drop(file);
        let back = within("a file read", nuclei::fs::read(&path))
            .await
            .unwrap();
        assert_eq!(back.len(), len, "soak: a file read back another length");
        check("a file", seed, 0, &back);
        totals.checked(len);
        within("a file removal", nuclei::fs::remove_file(&path))
            .await
            .unwrap();
        totals.files.fetch_add(1, Ordering::Relaxed);
    }
}

/// Sleeps, intervals and timeouts that lapse.
async fn timers(mut dice: Dice, totals: Arc<Totals>) {
    while totals.going() {
        let start = Instant::now();
        let nap = Duration::from_micros(dice.below(20_000));
        nuclei::time::sleep(nap).await;
        assert!(start.elapsed() >= nap, "soak: a sleep ended early");
        let lapsed = nuclei::time::timeout(
            Duration::from_micros(dice.below(1000)),
            core::future::pending::<()>(),
        )
        .await;
        assert!(lapsed.is_err());
        let mut ticks = nuclei::time::interval(Duration::from_micros(100 + dice.below(5000)));
        for _ in 0..3 {
            ticks.tick().await;
        }
        totals.timers.fetch_add(1, Ordering::Relaxed);
    }
}

/// The blocking pool, with work that blocks.
async fn blocking(mut dice: Dice, totals: Arc<Totals>) {
    while totals.going() {
        let nap = Duration::from_micros(dice.below(5000));
        let n = dice.below(1000);
        let got = within(
            "a blocking job",
            nuclei::spawn_blocking(move || {
                std::thread::sleep(nap);
                (0..n).sum::<u64>()
            }),
        )
        .await;
        assert_eq!(got, (0..n).sum::<u64>());
        totals.blocking.fetch_add(1, Ordering::Relaxed);
    }
}

/// What the process holds at one moment; `None` where the system does not
/// say (resident memory and threads come from `/proc`, on Linux).
#[derive(Clone, Copy, Debug)]
struct Sample {
    secs: u64,
    rss: Option<u64>,
    heap: isize,
    fds: Option<usize>,
    threads: Option<usize>,
    bytes: u64,
}

fn rss() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

fn count(dir: &str) -> Option<usize> {
    std::fs::read_dir(dir).ok().map(|d| d.count())
}

/// The descriptors open: from `/proc` on Linux, `/dev/fd` on macOS and the
/// BSDs; on Windows the process's handles.
#[cfg(unix)]
fn fds() -> Option<usize> {
    count("/proc/self/fd").or_else(|| count("/dev/fd"))
}

#[cfg(windows)]
fn fds() -> Option<usize> {
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessHandleCount(process: isize, count: *mut u32) -> i32;
    }
    let mut n = 0u32;
    // SAFETY: the pseudo handle of this process, and a place for the count.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut n) };
    (ok != 0).then_some(n as usize)
}

fn sample(start: Instant, totals: &Totals) -> Sample {
    Sample {
        secs: start.elapsed().as_secs(),
        rss: rss(),
        heap: LIVE.load(Ordering::Relaxed),
        fds: fds(),
        threads: count("/proc/self/task"),
        bytes: totals.bytes.load(Ordering::Relaxed),
    }
}

/// One of the quantities a sample holds, as a number, if the system said.
type Metric = fn(&Sample) -> Option<i128>;

/// Whether `metric` stayed flat after the warm-up: the highest of the
/// second half of the samples is no more than `slack` above the highest of
/// the first half. `None` when the system does not give it.
fn flat(samples: &[Sample], metric: Metric, slack: i128) -> Option<Result<(), (i128, i128)>> {
    let (early, late) = samples.split_at(samples.len() / 2);
    let early = early.iter().map(metric).collect::<Option<Vec<_>>>()?;
    let late = late.iter().map(metric).collect::<Option<Vec<_>>>()?;
    let (early, late) = (early.into_iter().max()?, late.into_iter().max()?);
    Some(if late > early + slack {
        Err((early, late))
    } else {
        Ok(())
    })
}

#[test]
#[ignore = "the soak runs an hour: NUCLEI_SOAK_SECS sets its length; run it with --ignored"]
fn everything_at_once_for_a_long_time_holds_steady_and_checks_every_byte() {
    let secs: u64 = std::env::var("NUCLEI_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600);
    let length = Duration::from_secs(secs);
    let rt = nuclei::Runtime::builder()
        .worker_threads(4)
        .pin_workers(false)
        .thread_keep_alive(Duration::from_secs(1))
        .build()
        .unwrap();
    let totals = Arc::new(Totals::default());
    let dir = std::env::temp_dir().join(format!("nuclei-soak-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let start = Instant::now();
    let mut work = Vec::new();
    let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    let server = rt.spawn(serve(listener, Arc::clone(&totals)));
    for k in 0..8u64 {
        let dice = || Dice(0x5eed_0000 + k * 7919 + 1);
        work.push(rt.spawn(churn(addr, dice(), Arc::clone(&totals))));
        if k < 3 {
            work.push(rt.spawn(stream(dice(), Arc::clone(&totals))));
            work.push(rt.spawn(datagrams(dice(), Arc::clone(&totals))));
            #[cfg(unix)]
            work.push(rt.spawn(unix_datagrams(dice(), Arc::clone(&totals))));
            work.push(rt.spawn(files(dir.clone(), dice(), Arc::clone(&totals))));
            work.push(rt.spawn(blocking(dice(), Arc::clone(&totals))));
        }
        work.push(rt.spawn(timers(dice(), Arc::clone(&totals))));
    }
    // A thread of std's that resets, half closes and never reads, against a
    // listener of nuclei's: its server tasks meet every peer fault.
    let faults = {
        let totals = Arc::clone(&totals);
        std::thread::spawn(move || {
            let mut dice = Dice(0xfa17);
            while totals.going() {
                let Ok(mut conn) = std::net::TcpStream::connect(addr) else {
                    continue;
                };
                let mut head = [0u8; 16];
                head[..8].copy_from_slice(&dice.next().to_le_bytes());
                head[8..].copy_from_slice(&(1u64 << 20).to_le_bytes());
                let _ = conn.write_all(&head);
                match dice.below(3) {
                    0 => drop(nuclei::Handle::new(conn).map(|h| h.set_zero_linger())),
                    1 => {
                        let _ = conn.shutdown(std::net::Shutdown::Write);
                        let _ = conn.read(&mut [0u8; 64]);
                    }
                    _ => drop(conn),
                }
                std::thread::sleep(Duration::from_millis(dice.below(20)));
            }
        })
    };
    let mut samples = Vec::new();
    let mut next = start + SAMPLE_EVERY;
    while start.elapsed() < length {
        let failed = work.iter().any(|t| t.is_finished()) || server.is_finished();
        if failed {
            break;
        }
        std::thread::sleep(
            next.saturating_duration_since(Instant::now())
                .min(Duration::from_millis(200)),
        );
        if Instant::now() >= next {
            samples.push(sample(start, &totals));
            next += SAMPLE_EVERY;
        }
    }
    totals.stop.store(true, Ordering::Relaxed);
    for task in work {
        rt.block_on(within("a workload's end", task));
    }
    faults.join().unwrap();
    totals.stop_serving.store(true, Ordering::Relaxed);
    rt.block_on(within("the server's end", server));
    let _ = std::fs::remove_dir_all(&dir);
    let backend = nuclei::stats()
        .shards
        .iter()
        .find(|s| s.worker.is_some())
        .map(|s| s.backend);
    drop(rt);
    let ran = start.elapsed();
    let warm = samples.len() / 5;
    let steady = &samples[warm..];
    let shown = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |v| v.to_string());
    let line = |s: &Sample| {
        format!(
            "{:>6}s rss {:>6} KiB heap {:>8} B fds {:>4} threads {:>3} checked {:>12} B",
            s.secs,
            shown(s.rss.map(|r| r / 1024)),
            s.heap,
            shown(s.fds.map(|f| f as u64)),
            shown(s.threads.map(|t| t as u64)),
            s.bytes
        )
    };
    let series: String = samples.iter().map(|s| line(s) + "\n").collect();
    let summary = format!(
        "soak: {ran:?} on {:?}, {} bytes checked, {} connections, {} resets, {} given up, {} datagrams, {} files, {} timers, {} blocking jobs, {} samples",
        backend,
        totals.bytes.load(Ordering::Relaxed),
        totals.connections.load(Ordering::Relaxed),
        totals.resets.load(Ordering::Relaxed),
        totals.cancelled.load(Ordering::Relaxed),
        totals.datagrams.load(Ordering::Relaxed),
        totals.files.load(Ordering::Relaxed),
        totals.timers.load(Ordering::Relaxed),
        totals.blocking.load(Ordering::Relaxed),
        samples.len(),
    );
    println!("{summary}");
    if let (Some(first), Some(last)) = (steady.first(), steady.last()) {
        println!(
            "after the warm-up: {}\n                   {}",
            line(first),
            line(last)
        );
    }
    assert!(
        ran >= length,
        "soak: a workload ended early, at {ran:?}\n{series}"
    );
    assert!(
        steady.len() >= 4,
        "soak: too few samples to judge\n{series}"
    );
    let checks: [(&str, Metric, i128); 4] = [
        ("resident memory", |s| s.rss.map(i128::from), 64 << 20),
        ("live heap bytes", |s| Some(s.heap as i128), 32 << 20),
        ("open descriptors", |s| s.fds.map(|f| f as i128), 64),
        ("threads", |s| s.threads.map(|t| t as i128), 8),
    ];
    let mut rising = Vec::new();
    for (name, metric, slack) in checks {
        match flat(steady, metric, slack) {
            None => println!("soak: {name} not measured on this system"),
            Some(Ok(())) => {}
            Some(Err((early, late))) => rising.push(format!(
                "{name}: at most {early} in the first half, {late} in the second"
            )),
        }
    }
    assert!(rising.is_empty(), "soak: {}\n{series}", rising.join("; "));
    let _ = io::stdout().flush();
}
