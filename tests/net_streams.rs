//! Stream reads at every size around the read path's thresholds, through traffic whose shape
//! changes: bytes that run ahead of the reads, then reads that keep up, in turn, on every driver.
//! Bytes written before the first read, both ends writing before they read, a peer that resets
//! mid-stream, reads given up mid-flight, many connections at once and a receive group held to its
//! least (`io_buffer_cap`) each have a case, and `random_traffic_across_the_read_path` draws all of
//! it at once from a seed. Every byte is checked where it lands, every step has a deadline, and a
//! watchdog ends the run within seconds of a hang the deadlines cannot see.
//!
//! The random case reads `NUCLEI_STREAMS_SEED` (printed as it starts) and `NUCLEI_STREAMS_ROUNDS`
//! (8 by default) from the environment; `NUCLEI_BACKEND` picks the backend.

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use futures_io::{AsyncRead, AsyncWrite};
use nuclei::Runtime;
use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream};

/// How long one read, write or round trip may take before the case fails as hung.
const STEP: Duration = Duration::from_secs(10);
/// How long the watchdog lets a case go with no step done before it ends the process.
const STALL: Duration = Duration::from_secs(15);

/// Read sizes on both sides of each threshold of the read path: the pool's size classes, the
/// ring's direct read and the receive group's buffers (16 KiB), and past them.
const SIZES: [usize; 19] = [
    1, 2, 63, 64, 65, 511, 512, 513, 4095, 4096, 4097, 12_287, 12_288, 16_383, 16_384, 16_385,
    65_535, 65_536, 65_537,
];

/// The steps a case finished, for the watchdog.
#[derive(Default)]
struct Dog(AtomicU64);

/// Runs `case` on a runtime of two workers (`cap` its `io_buffer_cap`) under a watchdog that ends
/// the process when no step finishes for [`STALL`].
fn watched(name: &str, cap: Option<usize>, case: impl AsyncFnOnce(Arc<Dog>)) {
    let mut builder = Runtime::builder().worker_threads(2).pin_workers(false);
    if let Some(cap) = cap {
        builder = builder.io_buffer_cap(cap);
    }
    let rt = builder.build().unwrap();
    let dog = Arc::new(Dog::default());
    let (done, wait) = channel::<()>();
    let watching = {
        let (dog, name) = (Arc::clone(&dog), name.to_owned());
        std::thread::spawn(move || {
            let (mut seen, mut since) = (dog.0.load(Ordering::Relaxed), Instant::now());
            while let Err(RecvTimeoutError::Timeout) = wait.recv_timeout(Duration::from_secs(1)) {
                let now = dog.0.load(Ordering::Relaxed);
                if now != seen {
                    (seen, since) = (now, Instant::now());
                } else if since.elapsed() > STALL {
                    eprintln!("{name}: no step finished in {STALL:?}, after {now} steps");
                    std::process::abort();
                }
            }
        })
    };
    rt.block_on(case(dog));
    drop(done);
    watching.join().unwrap();
}

/// `fut`, counted as a step done, or a failure naming `what` past [`STEP`].
async fn step<F: Future>(dog: &Dog, what: &str, fut: F) -> F::Output {
    match nuclei::time::timeout(STEP, fut).await {
        Ok(out) => {
            dog.0.fetch_add(1, Ordering::Relaxed);
            out
        }
        Err(_) => panic!("{what}: no answer in {STEP:?}"),
    }
}

/// The byte at `at` of stream `seed`.
fn byte_at(seed: u64, at: u64) -> u8 {
    let mut x = (seed ^ at.wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_add(at);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 29;
    x as u8
}

fn bytes(seed: u64, from: u64, len: usize) -> Vec<u8> {
    (0..len as u64).map(|i| byte_at(seed, from + i)).collect()
}

fn check(what: &str, seed: u64, from: u64, got: &[u8]) {
    if let Some(i) = (0..got.len()).find(|&i| got[i] != byte_at(seed, from + i as u64)) {
        panic!(
            "{what}: byte {} is wrong ({} read at {from})",
            from + i as u64,
            got.len()
        );
    }
}

/// A small generator of a case's choices.
#[derive(Clone)]
struct Dice(u64);

impl Dice {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    /// A size at or next to a threshold, or anywhere up to `most`.
    fn size(&mut self, most: usize) -> usize {
        let size = if self.below(2) == 0 {
            SIZES[self.below(SIZES.len() as u64) as usize]
        } else {
            let order = self.below(17);
            1 + self.below(1 << order) as usize
        };
        size.min(most)
    }
}

/// A stream kind both of whose ends are nuclei's.
trait Conn: AsyncRead + AsyncWrite + Unpin + Send + Sized + 'static {
    const NAME: &'static str;
    /// Two connected ends.
    fn pair() -> impl Future<Output = (Self, Self)> + Send;
    /// Closes this end so that its peer sees a reset where the system gives one: TCP with
    /// `SO_LINGER` 0, a Unix stream with bytes it never read (Linux resets it, the BSDs and
    /// macOS end it).
    fn reset(self);
}

impl Conn for TcpStream {
    const NAME: &'static str = "tcp";

    async fn pair() -> (Self, Self) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (conn, accepted) = nuclei::join!(TcpStream::connect(addr), listener.accept());
        (conn.unwrap(), accepted.unwrap().0)
    }

    fn reset(self) {
        self.set_zero_linger().unwrap();
    }
}

#[cfg(unix)]
impl Conn for nuclei::net::UnixStream {
    const NAME: &'static str = "unix";

    async fn pair() -> (Self, Self) {
        nuclei::net::UnixStream::pair().unwrap()
    }

    fn reset(self) {}
}

/// One phase of a script: `burst` bytes the peer writes without waiting, which run ahead of the
/// reads, then a round trip of each of `echoes`: the peer writes that many bytes, the reader reads
/// them all and writes them back, so its reads keep up.
#[derive(Clone, Debug)]
struct Phase {
    burst: usize,
    echoes: Vec<usize>,
}

/// What one connection carries: the peer's stream of bytes (`seed`), its phases, the reader's read
/// sizes (`read`, or drawn when `None`), every `give_up`-th read given up mid-flight (never at 0),
/// and a peer that resets once it wrote `reset` bytes, all of its phases being bursts.
#[derive(Clone, Debug)]
struct Script {
    seed: u64,
    phases: Vec<Phase>,
    read: Option<usize>,
    give_up: u64,
    reset: Option<u64>,
}

impl Script {
    fn fixed(seed: u64, read: usize, phases: Vec<Phase>) -> Self {
        Self {
            seed,
            phases,
            read: Some(read),
            give_up: 0,
            reset: None,
        }
    }

    fn total(&self) -> u64 {
        self.phases
            .iter()
            .map(|p| (p.burst + p.echoes.iter().sum::<usize>()) as u64)
            .sum()
    }
}

/// The phases of a scripted case reading `read` bytes at a time: three bursts, each well past a
/// read, and between them round trips of 64 bytes, each fitting a read of 64 or more.
fn bursts_and_echoes(read: usize) -> Vec<Phase> {
    let burst = (read * 64).clamp(4096, 256 * 1024);
    (0..3)
        .map(|_| Phase {
            burst,
            echoes: vec![64; 8],
        })
        .collect()
}

/// The end that reads under test: at `at` of the stream, reading at most `len` and at most what
/// the phase has left, every `give_up`-th read given up mid-flight first. `Ok(None)` for the end.
struct Reader<S> {
    conn: S,
    script: Script,
    dice: Dice,
    at: u64,
    reads: u64,
    buf: Vec<u8>,
}

impl<S: Conn> Reader<S> {
    fn what(&self) -> String {
        format!(
            "{} reader at {} of {:?}",
            S::NAME,
            self.at,
            (self.script.seed, self.script.read)
        )
    }

    /// One read of at most `left` bytes, checked: how many came, 0 at the end.
    async fn read(&mut self, dog: &Dog, left: usize) -> io::Result<usize> {
        let len = self
            .script
            .read
            .unwrap_or_else(|| self.dice.size(1 << 17))
            .min(left)
            .max(1);
        if self.buf.len() < len {
            self.buf.resize(len, 0);
        }
        self.reads += 1;
        if self.script.give_up != 0 && self.reads.is_multiple_of(self.script.give_up) {
            let late = self.reads.is_multiple_of(2 * self.script.give_up);
            if let Some(n) = self.give_up(len, late).await? {
                return Ok(n);
            }
        }
        let what = self.what();
        let n = step(dog, &what, self.conn.read(&mut self.buf[..len])).await?;
        check(&what, self.script.seed, self.at, &self.buf[..n]);
        self.at += n as u64;
        Ok(n)
    }

    /// A read of `len` polled once and dropped: at once, or `late`, after the shard ran its other
    /// work, so the driver may have done what the read waited for while the read never saw it.
    /// The next read loses nothing. What it read, if it finished on its poll.
    async fn give_up(&mut self, len: usize, late: bool) -> io::Result<Option<usize>> {
        let (seed, at) = (self.script.seed, self.at);
        let what = self.what();
        let got = {
            let mut fut = pin!(self.conn.read(&mut self.buf[..len]));
            let polled = fut.as_mut().poll(&mut Context::from_waker(Waker::noop()));
            match polled {
                Poll::Ready(result) => Some(result?),
                Poll::Pending => {
                    if late {
                        for _ in 0..4 {
                            nuclei::task::yield_now().await;
                        }
                    }
                    None
                }
            }
        };
        Ok(got.inspect(|&n| {
            check(&what, seed, at, &self.buf[..n]);
            self.at += n as u64;
        }))
    }

    /// Reads exactly `len` bytes of the stream.
    async fn take(&mut self, dog: &Dog, len: usize) -> io::Result<()> {
        let mut left = len;
        while left > 0 {
            match self.read(dog, left).await? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => left -= n,
            }
        }
        Ok(())
    }
}

/// Runs `script` over a fresh pair of `S`: the reader end checks every byte against the peer's
/// stream and writes each round trip's bytes back, the peer checks them, and each end sees the
/// other's end once all is through.
async fn carry<S: Conn>(dog: Arc<Dog>, script: Script) {
    let (ours, theirs) = S::pair().await;
    match script.reset {
        None => {
            let peer = nuclei::spawn(peer::<S>(Arc::clone(&dog), theirs, script.clone()));
            reader::<S>(&dog, ours, script).await;
            peer.await;
        }
        Some(at) => resets::<S>(dog, ours, theirs, script, at).await,
    }
}

async fn reader<S: Conn>(dog: &Dog, conn: S, script: Script) {
    let mut r = Reader {
        conn,
        dice: Dice::new(script.seed ^ 0x5eed),
        script: script.clone(),
        at: 0,
        reads: 0,
        buf: Vec::new(),
    };
    for phase in &script.phases {
        r.take(dog, phase.burst).await.unwrap();
        for &len in &phase.echoes {
            let from = r.at;
            r.take(dog, len).await.unwrap();
            let back = bytes(script.seed, from, len);
            step(dog, &r.what(), r.conn.write_all(&back)).await.unwrap();
        }
    }
    let what = r.what();
    assert_eq!(r.read(dog, 1).await.unwrap(), 0, "{what}: past the end");
    step(dog, &what, r.conn.shutdown()).await.unwrap();
}

async fn peer<S: Conn>(dog: Arc<Dog>, mut conn: S, script: Script) {
    let mut dice = Dice::new(script.seed ^ 0xbee5);
    let mut at = 0u64;
    let what = format!("{} peer of {:?}", S::NAME, (script.seed, script.read));
    for phase in &script.phases {
        let mut left = phase.burst;
        while left > 0 {
            let len = dice.size(1 << 17).min(left);
            step(&dog, &what, conn.write_all(&bytes(script.seed, at, len)))
                .await
                .unwrap();
            (at, left) = (at + len as u64, left - len);
        }
        for &len in &phase.echoes {
            step(&dog, &what, conn.write_all(&bytes(script.seed, at, len)))
                .await
                .unwrap();
            let mut back = vec![0u8; len];
            step(&dog, &what, conn.read_exact(&mut back)).await.unwrap();
            check(&what, script.seed, at, &back);
            at += len as u64;
        }
    }
    step(&dog, &what, conn.shutdown()).await.unwrap();
    let mut rest = [0u8; 1];
    assert_eq!(
        step(&dog, &what, conn.read(&mut rest)).await.unwrap(),
        0,
        "{what}: bytes past the reader's end"
    );
}

/// The peer writes its bursts and resets after `reset` bytes, with a byte of the reader's it never
/// read; the reader gets a prefix of the stream, every byte right, then the reset or the end.
async fn resets<S: Conn>(dog: Arc<Dog>, mut ours: S, mut theirs: S, script: Script, reset: u64) {
    step(&dog, "a poke", ours.write_all(&[1])).await.unwrap();
    let what = format!("{} resetting peer of {:?}", S::NAME, script.seed);
    let peer = {
        let dog = Arc::clone(&dog);
        let seed = script.seed;
        nuclei::spawn(async move {
            let mut dice = Dice::new(seed ^ 0xbee5);
            let mut at = 0u64;
            while at < reset {
                let len = (dice.size(1 << 17) as u64).min(reset - at) as usize;
                step(&dog, &what, theirs.write_all(&bytes(seed, at, len)))
                    .await
                    .unwrap();
                at += len as u64;
            }
            theirs.reset();
        })
    };
    let mut r = Reader {
        conn: ours,
        dice: Dice::new(script.seed ^ 0x5eed),
        script: script.clone(),
        at: 0,
        reads: 0,
        buf: Vec::new(),
    };
    let end = loop {
        match r.read(&dog, usize::MAX).await {
            Ok(0) => break None,
            Ok(_) => continue,
            Err(err) => break Some(err),
        }
    };
    peer.await;
    let what = r.what();
    assert!(r.at <= reset, "{what}: more than the peer wrote");
    if let Some(err) = end {
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
            ),
            "{what}: {err:?}"
        );
    } else {
        assert_eq!(
            r.at, reset,
            "{what}: an end before the bytes the peer wrote"
        );
    }
}

/// The least `io_buffer_cap`: on io_uring the default receive group (256 buffers of 16 KiB and
/// their descriptors, each mapped in whole pages) and one 512-byte buffer beside it; elsewhere 0
/// (as in tests/config_sizes.rs).
fn least_cap() -> usize {
    if nuclei::Proactor::backend() != nuclei::IoBackend::IoUring {
        return 0;
    }
    #[cfg(target_os = "linux")]
    {
        let config = nuclei::config::IoUringConfiguration::default();
        assert_eq!((config.recv_buffers, config.recv_buffer_size), (256, 16384));
        // SAFETY: a plain query.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let pages = |bytes: usize| bytes.div_ceil(page) * page;
        pages(256 * 16) + pages(256 * 16384) + 512
    }
    #[cfg(not(target_os = "linux"))]
    unreachable!("io_uring on a system that is not Linux")
}

/// Runs every script at once, each over a pair of its kind (`unix` picks Unix streams).
async fn all_at_once(dog: Arc<Dog>, scripts: Vec<(bool, Script)>) {
    let tasks: Vec<_> = scripts
        .into_iter()
        .map(|(unix, script)| {
            let dog = Arc::clone(&dog);
            nuclei::spawn(async move {
                #[cfg(unix)]
                if unix {
                    return carry::<nuclei::net::UnixStream>(dog, script).await;
                }
                let _ = unix;
                carry::<TcpStream>(dog, script).await
            })
        })
        .collect();
    for task in tasks {
        task.await;
    }
}

/// The stream kinds a case runs over.
fn kinds() -> &'static [bool] {
    if cfg!(unix) { &[false, true] } else { &[false] }
}

#[test]
fn bursts_and_round_trips_at_every_read_size_arrive_exactly() {
    for &unix in kinds() {
        watched("bursts and round trips", None, async |dog| {
            for (i, read) in SIZES.into_iter().enumerate() {
                let script = Script::fixed(i as u64 + 1, read, bursts_and_echoes(read));
                all_at_once(Arc::clone(&dog), vec![(unix, script)]).await;
            }
        });
    }
}

#[test]
fn reads_given_up_mid_flight_lose_nothing() {
    for &unix in kinds() {
        watched("reads given up", None, async |dog| {
            for (i, read) in [1, 64, 4096, 16_383, 16_385, 65_536]
                .into_iter()
                .enumerate()
            {
                for give_up in [1, 2, 3, 7] {
                    let mut script = Script::fixed(100 + i as u64, read, bursts_and_echoes(read));
                    script.give_up = give_up;
                    all_at_once(Arc::clone(&dog), vec![(unix, script)]).await;
                }
            }
        });
    }
}

#[test]
fn bytes_written_before_the_first_read_arrive_exactly() {
    // Small enough for every system's default socket buffers, so the writes finish unread.
    let room = |unix: bool| if unix { 6000 } else { 32 * 1024 };
    for &unix in kinds() {
        watched("bytes before the first read", None, async |dog| {
            for (i, read) in SIZES.into_iter().enumerate() {
                let seed = 200 + i as u64;
                let len = room(unix);
                #[cfg(unix)]
                if unix {
                    before_reads::<nuclei::net::UnixStream>(&dog, seed, read, len).await;
                    continue;
                }
                before_reads::<TcpStream>(&dog, seed, read, len).await;
            }
        });
    }
}

/// The peer writes `len` bytes and its end before the first read, which then takes them `read` at a
/// time; and both ends of another pair write before either reads.
async fn before_reads<S: Conn>(dog: &Dog, seed: u64, read: usize, len: usize) {
    let (mut ours, mut theirs) = S::pair().await;
    step(
        dog,
        "a write before reads",
        theirs.write_all(&bytes(seed, 0, len)),
    )
    .await
    .unwrap();
    step(dog, "an end before reads", theirs.shutdown())
        .await
        .unwrap();
    let mut buf = vec![0u8; read];
    let mut at = 0u64;
    loop {
        let what = format!("{} read of {read} at {at}", S::NAME);
        match step(dog, &what, ours.read(&mut buf)).await.unwrap() {
            0 => break,
            n => {
                check(&what, seed, at, &buf[..n]);
                at += n as u64;
            }
        }
    }
    assert_eq!(at, len as u64, "{}: what came before the end", S::NAME);

    let (mut a, mut b) = S::pair().await;
    let small = len.min(6000);
    step(dog, "a's write", a.write_all(&bytes(seed, 0, small)))
        .await
        .unwrap();
    step(dog, "b's write", b.write_all(&bytes(!seed, 0, small)))
        .await
        .unwrap();
    for (conn, from) in [(&mut a, !seed), (&mut b, seed)] {
        let mut got = vec![0u8; small];
        let mut at = 0;
        while at < small {
            let end = (at + read).min(small);
            let what = format!("{} crossed read of {read} at {at}", S::NAME);
            match step(dog, &what, conn.read(&mut got[at..end]))
                .await
                .unwrap()
            {
                0 => panic!("{what}: an end"),
                n => at += n,
            }
        }
        check("a crossed write", from, 0, &got);
    }
}

#[test]
fn a_peer_that_resets_mid_stream_leaves_a_prefix_and_the_reset() {
    for &unix in kinds() {
        watched("resets", None, async |dog| {
            for (i, read) in [1, 64, 4096, 16_384, 65_537].into_iter().enumerate() {
                for reset in [1, 4097, 200_000] {
                    let script = Script {
                        reset: Some(reset),
                        give_up: 5,
                        ..Script::fixed(300 + i as u64, read, Vec::new())
                    };
                    all_at_once(Arc::clone(&dog), vec![(unix, script)]).await;
                }
            }
        });
    }
}

#[test]
fn many_streams_at_once() {
    watched("many streams", None, async |dog| {
        let scripts = (0..48)
            .map(|i| {
                let read = SIZES[i % SIZES.len()];
                let unix = kinds()[i % kinds().len()];
                (
                    unix,
                    Script::fixed(400 + i as u64, read, bursts_and_echoes(read)),
                )
            })
            .collect();
        all_at_once(dog, scripts).await;
    });
}

#[test]
fn a_receive_group_at_its_least_holds_no_reader_up() {
    watched("the least receive group", Some(least_cap()), async |dog| {
        let scripts = (0..24)
            .map(|i| {
                let read = [64, 100, 4096][i % 3];
                let unix = kinds()[i % kinds().len()];
                let phases = vec![
                    Phase {
                        burst: 512 * 1024,
                        echoes: vec![64; 4],
                    };
                    2
                ];
                let mut script = Script::fixed(500 + i as u64, read, phases);
                script.give_up = 11;
                (unix, script)
            })
            .collect();
        all_at_once(dog, scripts).await;
    });
}

#[test]
fn random_traffic_across_the_read_path() {
    let seed = std::env::var("NUCLEI_STREAMS_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
        });
    let rounds: u64 = std::env::var("NUCLEI_STREAMS_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    eprintln!("random_traffic_across_the_read_path: NUCLEI_STREAMS_SEED={seed}, {rounds} rounds");
    let mut dice = Dice::new(seed);
    for round in 0..rounds {
        let cap = (round % 2 == 1).then(least_cap);
        let scripts: Vec<(bool, Script)> = (0..1 + dice.below(16))
            .map(|_| {
                let unix = kinds()[dice.below(kinds().len() as u64) as usize];
                (unix, drawn(&mut dice))
            })
            .collect();
        let name = format!("random round {round} of seed {seed}");
        watched(&name, cap, async |dog| all_at_once(dog, scripts).await);
    }
}

/// A script drawn from `dice`: up to four phases of bursts up to 256 KiB and up to a dozen round
/// trips, reads of drawn sizes, some given up, and one script in eight a peer that resets.
fn drawn(dice: &mut Dice) -> Script {
    let give_up = [0, 0, 5, 13][dice.below(4) as usize];
    let seed = dice.next();
    if dice.below(8) == 0 {
        return Script {
            seed,
            phases: Vec::new(),
            read: None,
            give_up,
            reset: Some(1 + dice.below(300_000)),
        };
    }
    let phases = (0..1 + dice.below(4))
        .map(|_| Phase {
            burst: dice.size(256 * 1024),
            echoes: (0..dice.below(13)).map(|_| dice.size(64 * 1024)).collect(),
        })
        .collect();
    let script = Script {
        seed,
        phases,
        read: None,
        give_up,
        reset: None,
    };
    debug_assert!(script.total() > 0);
    script
}
