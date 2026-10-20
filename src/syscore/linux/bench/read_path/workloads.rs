//! The writer, the destination buffers and the two workloads.

use super::*;

/// The deterministic byte stream the writer sends: byte `i` of the stream is `pattern()[i % LEN]`.
pub fn pattern() -> &'static [u8] {
    static P: OnceLock<Vec<u8>> = OnceLock::new();
    P.get_or_init(|| {
        (0..PATTERN_LEN)
            .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect()
    })
}

/// A position in the pattern stream.
#[derive(Default)]
pub struct Cursor(usize);

impl Cursor {
    /// The next at most `max` bytes of the stream, never crossing the pattern's end.
    pub fn next(&mut self, max: usize) -> &'static [u8] {
        let end = (self.0 + max).min(PATTERN_LEN);
        let chunk = &pattern()[self.0..end];
        self.0 = end % PATTERN_LEN;
        chunk
    }

    /// Asserts `got` is the next `got.len()` bytes of the stream.
    pub fn verify(&mut self, mut got: &[u8]) {
        while !got.is_empty() {
            let start = self.0;
            let want = self.next(got.len());
            let (head, rest) = got.split_at(want.len());
            assert!(head == want, "stream diverges at pattern offset {start}");
            got = rest;
        }
    }
}

/// Sends `n` stream bytes, blocking.
pub fn send_stream(sock: &mut TcpStream, cur: &mut Cursor, mut n: usize) -> io::Result<()> {
    while n > 0 {
        let chunk = cur.next(n.min(WRITE_CHUNK));
        sock.write_all(chunk)?;
        n -= chunk.len();
    }
    Ok(())
}

/// The writer: sends the stream until `limit` bytes are out (then closes its side) or `stop` is
/// set. An error after `stop` is the reader closing the connection and is not one.
pub fn feed(mut sock: TcpStream, limit: u64, stop: &AtomicBool) -> io::Result<()> {
    let (mut cur, mut sent) = (Cursor::default(), 0u64);
    let res = (|| {
        while sent < limit && !stop.load(Ordering::Relaxed) {
            let n = (limit - sent).min(WRITE_CHUNK as u64) as usize;
            send_stream(&mut sock, &mut cur, n)?;
            sent += n as u64;
        }
        sock.shutdown(Shutdown::Write)
    })();
    if stop.load(Ordering::Relaxed) {
        Ok(())
    } else {
        res
    }
}

/// The ping-pong peer: reads `s` bytes, answers with `s` stream bytes, until the client closes.
pub fn answer(mut sock: TcpStream, s: usize) -> io::Result<()> {
    let (mut req, mut cur) = (vec![0u8; s], Cursor::default());
    loop {
        match sock.read_exact(&mut req) {
            Ok(()) => send_stream(&mut sock, &mut cur, s)?,
            Err(e) if closed(&e) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

pub fn closed(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset
    )
}

/// A connected loopback pair: the nonblocking reader side and the blocking peer.
pub fn connected_pair() -> io::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let peer = TcpStream::connect(listener.local_addr()?)?;
    let (conn, _) = listener.accept()?;
    for s in [&conn, &peer] {
        s.set_nodelay(true)?;
    }
    conn.set_nonblocking(true)?;
    Ok((conn, peer))
}

/// Sends all of `buf` on the nonblocking `fd`, waiting for writability when the socket is full.
pub fn send_all(fd: RawFd, mut buf: &[u8], c: &mut Counters) -> io::Result<()> {
    while !buf.is_empty() {
        c.send += 1;
        // SAFETY: `buf` is valid for `buf.len()` readable bytes.
        let n = unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), libc::MSG_NOSIGNAL) };
        if n >= 0 {
            buf = &buf[n as usize..];
            continue;
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            ErrorKind::WouldBlock => {
                let mut p = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                c.send += 1;
                // SAFETY: `p` is one valid pollfd.
                unsafe { libc::poll(&mut p, 1, -1) };
            }
            ErrorKind::Interrupted => {}
            _ => return Err(e),
        }
    }
    Ok(())
}

/// Pool for read size `s`: buffers of the read size (a power of two from 4 KiB to 256 KiB), and
/// four times the smallest count (doubling from 16, up to a 64 MiB pool) at which a hot and a cold
/// probe stream both never find the pool empty. The measured tables report any starvation that
/// still happens.
pub fn size_pool(s: usize, cpus: Cpus, cold: usize) -> io::Result<PoolCfg> {
    let buf_size = s.next_power_of_two().clamp(4 << 10, 256 << 10) as u32;
    let lim = StreamLimits {
        bytes: 1 << 30,
        secs: 3.0,
    };
    let mut count = 16u16;
    while u64::from(count) * u64::from(buf_size) < 64 << 20 {
        let cfg = PoolCfg { buf_size, count };
        let mut starved = 0;
        for dest in [None, Some(cold)] {
            starved += stream_run(Path::Pool, s, dest, cfg, &lim, Some(cpus))?
                .c
                .enobufs;
        }
        if starved == 0 {
            break;
        }
        count *= 2;
    }
    Ok(PoolCfg {
        buf_size,
        count: (u32::from(count) * 4).min(32768) as u16,
    })
}

/// The caller's destination buffers: one (hot) or a rotation covering `cold` bytes.
pub struct Dest {
    pub buf: Vec<u8>,
    pub s: usize,
    pub n: usize,
    pub i: usize,
}

impl Dest {
    pub fn new(s: usize, cold: Option<usize>) -> Self {
        let n = cold.map_or(1, |b| b.div_ceil(s));
        // A non-zero fill touches every page now, so no page fault lands inside a timed run.
        Self {
            buf: vec![0xA5; n * s],
            s,
            n,
            i: 0,
        }
    }

    pub fn next(&mut self) -> &mut [u8] {
        let o = self.i * self.s;
        self.i = if self.i + 1 == self.n { 0 } else { self.i + 1 };
        &mut self.buf[o..o + self.s]
    }
}

pub struct StreamRun {
    pub wall_s: f64,
    pub cpu_ns: u64,
    pub c: Counters,
}

pub struct StreamLimits {
    pub bytes: u64,
    pub secs: f64,
}

/// One streaming run: the writer feeds continuously, the reader issues reads of `s` bytes until
/// the byte target or the time limit. A reader that hits end of stream early is an error.
pub fn stream_run(
    path: Path,
    s: usize,
    cold: Option<usize>,
    cfg: PoolCfg,
    lim: &StreamLimits,
    cpus: Option<Cpus>,
) -> io::Result<StreamRun> {
    let (conn, peer) = connected_pair()?;
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            cpus.map_or(Ok(()), |c| pin(c.writer))?;
            feed(peer, u64::MAX, &stop)
        })
    };
    let (target, secs) = (lim.bytes, lim.secs);
    let reader = thread::spawn(move || -> io::Result<StreamRun> {
        cpus.map_or(Ok(()), |c| pin(c.reader))?;
        let mut r = Reader::new(path, conn.as_raw_fd(), cfg)?;
        let mut dest = Dest::new(s, cold);
        let (t0, cpu0) = (Instant::now(), thread_cpu_ns());
        let (mut bytes, mut reads) = (0u64, 0u64);
        while bytes < target {
            let n = r.read(dest.next())?;
            if n == 0 {
                return Err(ErrorKind::UnexpectedEof.into());
            }
            bytes += n as u64;
            reads += 1;
            if reads & 63 == 0 && t0.elapsed().as_secs_f64() >= secs {
                break;
            }
        }
        let (wall_s, cpu_ns) = (t0.elapsed().as_secs_f64(), thread_cpu_ns() - cpu0);
        stop.store(true, Ordering::Relaxed);
        let c = r.counters();
        (c.reads, c.bytes) = (reads, bytes);
        Ok(StreamRun {
            wall_s,
            cpu_ns,
            c: *c,
        })
    });
    let run = reader.join().expect("the reader thread panicked");
    let writer = writer.join().expect("the writer thread panicked");
    let run = run?;
    writer.map(|()| run)
}

pub struct PpRun {
    pub p50_us: f64,
    pub p99_us: f64,
    pub cpu_us_rt: f64,
    pub rts: u64,
    pub c: Counters,
}

/// One ping-pong run: `s`-byte request, `s`-byte answer, timed per round trip, until the round
/// trip count or the time limit. The reader side is the measured one.
pub fn pingpong_run(
    path: Path,
    s: usize,
    max_rts: usize,
    cfg: PoolCfg,
    cpus: Option<Cpus>,
    verify: bool,
) -> io::Result<PpRun> {
    let (conn, peer) = connected_pair()?;
    let peer = thread::spawn(move || {
        cpus.map_or(Ok(()), |c| pin(c.writer))?;
        answer(peer, s)
    });
    let reader = thread::spawn(move || -> io::Result<PpRun> {
        cpus.map_or(Ok(()), |c| pin(c.reader))?;
        let fd = conn.as_raw_fd();
        let mut r = Reader::new(path, fd, cfg)?;
        let (req, mut dst) = (vec![0x5Au8; s], vec![0u8; s]);
        let mut cur = Cursor::default();
        let mut round_trip = |r: &mut Reader| -> io::Result<()> {
            send_all(fd, &req, r.counters())?;
            let mut got = 0;
            while got < s {
                match r.read(&mut dst[got..])? {
                    0 => return Err(ErrorKind::UnexpectedEof.into()),
                    n => got += n,
                }
                r.counters().reads += 1;
            }
            if verify {
                cur.verify(&dst);
            }
            Ok(())
        };
        for _ in 0..PP_WARMUP.min(max_rts) {
            round_trip(&mut r)?;
        }
        *r.counters() = Counters::default();
        let mut lat = Vec::with_capacity(max_rts);
        let (t0, cpu0) = (Instant::now(), thread_cpu_ns());
        while lat.len() < max_rts {
            let t = Instant::now();
            round_trip(&mut r)?;
            lat.push(t.elapsed().as_nanos() as u32);
            if lat.len() & 255 == 0 && t0.elapsed().as_secs_f64() >= PP_MAX_SECS {
                break;
            }
        }
        let cpu_ns = thread_cpu_ns() - cpu0;
        lat.sort_unstable();
        let pct = |p: usize| f64::from(lat[(lat.len() - 1) * p / 100]) / 1_000.0;
        Ok(PpRun {
            p50_us: pct(50),
            p99_us: pct(99),
            cpu_us_rt: cpu_ns as f64 / lat.len() as f64 / 1_000.0,
            rts: lat.len() as u64,
            c: *r.counters(),
        })
    });
    let run = reader.join().expect("the reader thread panicked");
    let peer = peer.join().expect("the peer thread panicked");
    let run = run?;
    peer.map(|()| run)
}
