//! Durable file IO rows, one runtime per process: `fsbench <runtime> <row> key=value..`.
//!
//! Runtimes: `nuclei` (its backend from NUCLEI_BACKEND), `tokio` (tokio::fs on its
//! blocking pool, a current-thread runtime), `monoio` (its io_uring fs) and `glommio`
//! (BufferedFile for the buffered rows, DmaFile for the O_DIRECT ones).
//!
//! Rows. Every op is timed alone, and `dur=` seconds bound the run:
//! - `sync sync=fsync|fdatasync size=B`: B bytes written, then the sync, front to back
//!   over a prepared file (overwrites, wrapping at its end);
//! - `direct_write size=B`, `direct_read size=B`: O_DIRECT at random B-aligned offsets
//!   of a prepared file, from page-aligned memory wherever the API takes the caller's;
//! - `append sync=fsync|fdatasync group=N`: 4 KiB appends to a new file and one sync
//!   per N of them, the N appends and their sync being one op; at most 1 GiB;
//! - `writers_one`, `writers_many`: 16 tasks, each writing 4 KiB then fdatasync over
//!   its own region of one prepared file, or over a file of its own;
//! - `busy_pool jobs=K`: the `sync sync=fdatasync size=4096` row beside K tasks that
//!   each keep one 1 ms CPU-bound job in flight on the runtime's blocking pool.
//!
//! The data is pseudo-random (incompressible), the same for every runtime, as are the
//! offsets. Prints one `key=value` line, or `skip="reason"` when the runtime's API has
//! no call for the row. Run under `perf stat -D -1 --control fifo:CTL,ACK` with
//! FSBENCH_PERF_CTL and FSBENCH_PERF_ACK naming the two fifos, the counters cover
//! only the timed loop.

mod on_glommio;
mod on_monoio;
mod on_nuclei;
mod on_tokio;

use hdrhistogram::Histogram;
use std::alloc::{Layout, alloc, dealloc};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

pub const WRITERS: usize = 16;
pub const APPEND: usize = 4096;
pub const LOG_MAX: u64 = 1 << 30;
pub const JOB: Duration = Duration::from_millis(1);
pub const ALIGN: usize = 4096;
const SEED: u64 = 0x2545_f491_4f6c_dd1d;
const MAX_NS: u64 = 60_000_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    Fsync,
    Fdatasync,
}

#[derive(Clone, Copy)]
pub enum Row {
    Sync { kind: SyncKind, size: usize },
    DirectWrite { size: usize },
    DirectRead { size: usize },
    Append { kind: SyncKind, group: usize },
    WritersOne,
    WritersMany,
    BusyPool { jobs: usize },
}

impl Row {
    fn is_direct(self) -> bool {
        matches!(self, Row::DirectWrite { .. } | Row::DirectRead { .. })
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub dir: PathBuf,
    pub dur: Duration,
    pub file_bytes: u64,
}

impl Ctx {
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn many(&self, i: usize) -> PathBuf {
        self.path(&format!("data{i}"))
    }

    pub fn region(&self) -> u64 {
        self.file_bytes / WRITERS as u64
    }
}

pub enum Run {
    Done(Outcome),
    Skip(String),
}

/// Per-op latencies and counts of one task or a whole run.
pub struct Meter {
    hist: Histogram<u64>,
    ops: u64,
    bytes: u64,
}

impl Default for Meter {
    fn default() -> Meter {
        Meter {
            hist: Histogram::new_with_max(MAX_NS, 3).expect("valid bounds"),
            ops: 0,
            bytes: 0,
        }
    }
}

impl Meter {
    pub fn record(&mut self, since: Instant, bytes: usize) {
        let ns = (since.elapsed().as_nanos() as u64).clamp(1, MAX_NS);
        self.hist.saturating_record(ns);
        self.ops += 1;
        self.bytes += bytes as u64;
    }

    pub fn merge(&mut self, other: Meter) {
        self.hist.add(other.hist).expect("same bounds");
        self.ops += other.ops;
        self.bytes += other.bytes;
    }
}

pub struct Outcome {
    meter: Meter,
    secs: f64,
    io: (u64, u64),
    pub jobs: u64,
    pub note: Option<String>,
}

/// The timed part of a run: the perf counters and the process's storage IO run
/// from `begin` to `finish`.
pub struct Window {
    start: Instant,
    pub end: Instant,
    io: (u64, u64),
}

impl Window {
    pub fn begin(ctx: &Ctx) -> Window {
        perf("enable");
        let io = proc_io();
        let start = Instant::now();
        Window {
            start,
            end: start + ctx.dur,
            io,
        }
    }

    pub fn over(&self) -> bool {
        Instant::now() >= self.end
    }

    pub fn finish(self, meter: Meter) -> Outcome {
        let secs = self.start.elapsed().as_secs_f64();
        let io = proc_io();
        perf("disable");
        Outcome {
            meter,
            secs,
            io: (
                io.0.saturating_sub(self.io.0),
                io.1.saturating_sub(self.io.1),
            ),
            jobs: 0,
            note: None,
        }
    }
}

fn perf(cmd: &str) {
    let (Ok(ctl), Ok(ack)) = (
        std::env::var("FSBENCH_PERF_CTL"),
        std::env::var("FSBENCH_PERF_ACK"),
    ) else {
        return;
    };
    let open = |p: &str| std::fs::OpenOptions::new().read(true).write(true).open(p);
    let sent = open(&ctl).and_then(|mut c| c.write_all(format!("{cmd}\n").as_bytes()));
    let acked = sent.and_then(|()| {
        let mut buf = [0u8; 8];
        open(&ack)?.read(&mut buf).map(|_| ())
    });
    if let Err(e) = acked {
        fail(&format!("perf control {cmd}: {e}"));
    }
}

/// Bytes this process read from and wrote to storage (`/proc/self/io`).
fn proc_io() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/io").unwrap_or_default();
    let field = |name: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    };
    (field("read_bytes:"), field("write_bytes:"))
}

/// The xorshift64 stream every runtime draws its data and offsets from.
pub struct Rng(u64);

impl Rng {
    pub fn new(stream: u64) -> Rng {
        Rng(SEED ^ stream.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub fn step(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.step().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

pub fn random(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    Rng::new(len as u64).fill(&mut v);
    v
}

/// Random `size`-aligned offsets inside the prepared file.
pub struct Offsets {
    rng: Rng,
    slots: u64,
    size: u64,
}

impl Offsets {
    pub fn new(ctx: &Ctx, size: usize) -> Offsets {
        Offsets {
            rng: Rng::new(7),
            slots: (ctx.file_bytes / size as u64).max(1),
            size: size as u64,
        }
    }

    pub fn pick(&mut self) -> u64 {
        self.rng.step() % self.slots * self.size
    }
}

/// Page-aligned memory for the O_DIRECT rows of APIs that take the caller's buffer.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
    init: usize,
}

// SAFETY: the buffer owns its memory and has no interior sharing.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub fn random(len: usize) -> AlignedBuf {
        let mut b = AlignedBuf::zeroed(len);
        Rng::new(len as u64).fill(b.as_mut_slice());
        b
    }

    pub fn zeroed(len: usize) -> AlignedBuf {
        let layout = Layout::from_size_align(len.max(ALIGN), ALIGN).expect("valid layout");
        // SAFETY: the layout has a non-zero size.
        let ptr = NonNull::new(unsafe { alloc(layout) }).expect("allocation");
        // SAFETY: `ptr` holds `len` writable bytes.
        unsafe { ptr.as_ptr().write_bytes(0, len) };
        AlignedBuf {
            ptr,
            len,
            init: len,
        }
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn capacity(&self) -> usize {
        self.len
    }

    pub fn init(&self) -> usize {
        self.init
    }

    /// # Safety
    ///
    /// The first `n` bytes must be initialized.
    pub unsafe fn set_init(&mut self, n: usize) {
        self.init = n.min(self.len);
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the buffer holds `len` initialized bytes.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the buffer holds `len` initialized bytes, borrowed exclusively.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len.max(ALIGN), ALIGN).expect("valid layout");
        // SAFETY: allocated in `zeroed` with this layout.
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
    }
}

/// Burns `JOB` of CPU, the blocking-pool work of the `busy_pool` row.
pub fn cpu_job() -> u64 {
    let start = Instant::now();
    let mut rng = Rng::new(3);
    let mut acc = 0u64;
    while start.elapsed() < JOB {
        for _ in 0..256 {
            acc = acc.wrapping_add(rng.step());
        }
    }
    std::hint::black_box(acc)
}

fn prepare_file(path: &PathBuf, len: u64, stream: u64) -> io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    let mut rng = Rng::new(stream);
    let mut chunk = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        let n = left.min(chunk.len() as u64) as usize;
        rng.fill(&mut chunk[..n]);
        f.write_all(&chunk[..n])?;
        left -= n as u64;
    }
    f.sync_all()?;
    // A buffered fallback in a direct row would show as pages cached afterwards.
    // SAFETY: a plain advisory call on an open descriptor.
    unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    Ok(())
}

fn prepare(row: Row, ctx: &Ctx) -> io::Result<()> {
    match row {
        Row::Append { .. } => Ok(()),
        Row::WritersMany => {
            (0..WRITERS).try_for_each(|i| prepare_file(&ctx.many(i), ctx.region(), i as u64))
        }
        _ => prepare_file(&ctx.path("data"), ctx.file_bytes, 0),
    }
}

/// Pages of the prepared file in the page cache; zero after a run that was direct.
fn cached_pages(ctx: &Ctx) -> io::Result<u64> {
    let f = std::fs::File::open(ctx.path("data"))?;
    let len = f.metadata()?.len() as usize;
    if len == 0 {
        return Ok(0);
    }
    // SAFETY: a read-only shared mapping of an open file, never dereferenced, unmapped below.
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            0,
        );
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let page = libc::sysconf(libc::_SC_PAGESIZE) as usize;
        let mut vec = vec![0u8; len.div_ceil(page)];
        let rc = libc::mincore(p, len, vec.as_mut_ptr());
        libc::munmap(p, len);
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(vec.iter().filter(|&&b| b & 1 != 0).count() as u64)
    }
}

fn cleanup(ctx: &Ctx) {
    let _ = std::fs::remove_file(ctx.path("data"));
    let _ = std::fs::remove_file(ctx.path("log"));
    for i in 0..WRITERS {
        let _ = std::fs::remove_file(ctx.many(i));
    }
}

fn fail(msg: &str) -> ! {
    println!("error=\"{msg}\"");
    std::process::exit(1);
}

fn usage() -> ! {
    eprintln!(
        "usage: fsbench nuclei|tokio|monoio|glommio \
         sync|direct_write|direct_read|append|writers_one|writers_many|busy_pool \
         dir=PATH dur=S [size=B] [sync=fsync|fdatasync] [group=N] [jobs=K] [file_mib=M]"
    );
    std::process::exit(2);
}

fn parse() -> (String, Row, Ctx) {
    let args: Vec<String> = std::env::args().collect();
    let (Some(rt), Some(row)) = (args.get(1), args.get(2)) else {
        usage()
    };
    let kv: Vec<(&str, &str)> = args[3..]
        .iter()
        .map(|a| a.split_once('=').unwrap_or_else(|| usage()))
        .collect();
    let get = |k: &str| kv.iter().rev().find(|(key, _)| *key == k).map(|(_, v)| *v);
    let num = |k: &str, d: f64| get(k).map_or(d, |v| v.parse().unwrap_or_else(|_| usage()));
    let kind = match get("sync").unwrap_or("fdatasync") {
        "fsync" => SyncKind::Fsync,
        "fdatasync" => SyncKind::Fdatasync,
        _ => usage(),
    };
    let size = num("size", 4096.0) as usize;
    let row = match row.as_str() {
        "sync" => Row::Sync { kind, size },
        "direct_write" => Row::DirectWrite { size },
        "direct_read" => Row::DirectRead { size },
        "append" => Row::Append {
            kind,
            group: (num("group", 1.0) as usize).max(1),
        },
        "writers_one" => Row::WritersOne,
        "writers_many" => Row::WritersMany,
        "busy_pool" => Row::BusyPool {
            jobs: num("jobs", 4.0) as usize,
        },
        _ => usage(),
    };
    if size == 0 || !size.is_multiple_of(ALIGN) {
        fail(&format!(
            "size={size} must be a positive multiple of {ALIGN}"
        ));
    }
    let ctx = Ctx {
        dir: PathBuf::from(get("dir").unwrap_or_else(|| usage())),
        dur: Duration::from_secs_f64(num("dur", 5.0)),
        file_bytes: (num("file_mib", 64.0) as u64) << 20,
    };
    if ctx.file_bytes < (WRITERS * ALIGN) as u64 || ctx.file_bytes < size as u64 {
        fail("file_mib is too small for the row");
    }
    (rt.clone(), row, ctx)
}

fn main() {
    let (rt, row, ctx) = parse();
    if let Err(e) = std::fs::create_dir_all(&ctx.dir).and_then(|()| prepare(row, &ctx)) {
        cleanup(&ctx);
        fail(&format!("preparing the files: {e}"));
    }
    let run = match rt.as_str() {
        "nuclei" => on_nuclei::run(row, &ctx),
        "tokio" => on_tokio::run(row, &ctx),
        "monoio" => on_monoio::run(row, &ctx),
        "glommio" => on_glommio::run(row, &ctx),
        _ => usage(),
    };
    let cached = if row.is_direct() {
        cached_pages(&ctx).ok()
    } else {
        None
    };
    cleanup(&ctx);
    let out = match run {
        Ok(Run::Done(out)) => out,
        Ok(Run::Skip(reason)) => {
            println!("skip=\"{reason}\"");
            return;
        }
        Err(e) => fail(&e.to_string()),
    };
    let m = &out.meter;
    let us = |q: f64| {
        if m.ops == 0 {
            "na".into()
        } else {
            format!("{:.1}", m.hist.value_at_quantile(q) as f64 / 1e3)
        }
    };
    let per_op = |v: u64| {
        if m.ops == 0 {
            "na".into()
        } else {
            format!("{:.0}", v as f64 / m.ops as f64)
        }
    };
    let mut line = format!(
        "ops={} secs={:.3} ops_s={:.1} mib_s={:.2} p50_us={} p99_us={} p999_us={} max_us={} \
         io_rd_per_op={} io_wr_per_op={}",
        m.ops,
        out.secs,
        m.ops as f64 / out.secs,
        m.bytes as f64 / out.secs / (1 << 20) as f64,
        us(0.5),
        us(0.99),
        us(0.999),
        if m.ops == 0 {
            "na".into()
        } else {
            format!("{:.1}", m.hist.max() as f64 / 1e3)
        },
        per_op(out.io.0),
        per_op(out.io.1),
    );
    if let Some(pages) = cached {
        line += &format!(" cached_pages_after={pages}");
    }
    if let Row::BusyPool { .. } = row {
        line += &format!(" jobs_s={:.0}", out.jobs as f64 / out.secs);
    }
    if let Some(note) = &out.note {
        line += &format!(" note=\"{note}\"");
    }
    println!("{line}");
    if row.is_direct() && cached.is_some_and(|p| p > 0) {
        eprintln!("the direct row left pages in the page cache: some IO did not bypass it");
    }
}
