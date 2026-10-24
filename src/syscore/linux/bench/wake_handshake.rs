//! The remote wake handshake. A shard parks in `epoll_wait` while foreign threads push to its inbox
//! and signal its eventfd only when `parked` is set; measures the three store-load barrier
//! policies (A, B, C) on the park/wake Dekker pair.
//!
//! Workload: each waker sends bursts of 1..=8 messages, waits for the shard to acknowledge the
//! whole burst (closed loop, so a lost wakeup stalls the waker until the 100 ms `epoll_wait`
//! timeout and is counted), then pauses a random gap. The first message of a burst usually finds
//! the shard parked; the rest race its drain. A separate litmus drives the same pair in lockstep,
//! because the throughput workload only reaches the nanosecond race window by chance.
//! The control (C) keeps a compiler fence so a lost wakeup can only be hardware reordering.

use super::*;

const BURST_MAX: u64 = 8;
const FINE_NS: u64 = 400;
const MICRO_ITERS: usize = 1_000_000;
const LITMUS_ROUNDS: u64 = 1_000_000;
/// Random per-side skew, in iterations of an empty loop (about 1 ns each).
const LITMUS_SKEW: u64 = 300;
const TS_SHIFT: u32 = 26;
const W_SHIFT: u32 = 21;
const SEQ_MASK: u64 = (1 << W_SHIFT) - 1;
const TS_LIMIT: u64 = 1 << (64 - TS_SHIFT);
/// A message found in the inbox after a sleep that began this long after its push is "late".
const LATE_NS: u64 = 20_000;
const RUN_LIMIT_NS: u64 = 200_000_000_000;

/// Message layout: push timestamp (38 bits, ns since run start) | waker (5) | sequence (21).
fn pack(ts: u64, w: usize, seq: u64) -> u64 {
    assert!(
        ts < TS_LIMIT && seq <= SEQ_MASK && w < 32,
        "message field overflow"
    );
    (ts << TS_SHIFT) | ((w as u64) << W_SHIFT) | seq
}

fn unpack(m: u64) -> (u64, usize, u64) {
    (m >> TS_SHIFT, ((m >> W_SHIFT) & 31) as usize, m & SEQ_MASK)
}

enum Park {
    /// The re-check found work.
    Skipped,
    Woken,
    /// `lost` when the inbox was non-empty on return: a message was pushed and nobody signalled.
    TimedOut {
        lost: bool,
    },
}

/// Per-park bookkeeping: the cost of each attempt and when the shard last went to sleep.
#[derive(Default)]
struct Parker {
    cost: Vec<u32>,
    slept_at: u64,
}

impl Wake<u64> {
    /// One park attempt; its cost (publish, fence, re-check) is recorded in `p`.
    fn park<P: Protocol>(&self, p: &mut Parker, timeout_ms: i32, clock: Clock) -> Park {
        let t0 = clock.ns();
        let empty = self.arm::<P>();
        let t1 = clock.ns();
        p.cost.push((t1 - t0) as u32);
        if !empty {
            return Park::Skipped;
        }
        p.slept_at = t1;
        match self.wait(&mut [EMPTY_EV; 1], timeout_ms) {
            0 => Park::TimedOut {
                lost: !self.inbox.is_empty(),
            },
            _ => Park::Woken,
        }
    }
}

struct WakeCfg {
    wakers: usize,
    per_waker: u64,
    cpus: Option<Vec<usize>>,
    gap_ns: u64,
    /// Stop a run once this many lost wakeups (100 ms stalls) were seen.
    lost_abort: u64,
}

struct WakeCtl {
    clock: Clock,
    acked: Vec<CachePadded<AtomicU64>>,
    stop: AtomicBool,
}

struct ShardOut {
    park: Vec<u32>,
    lat: Vec<u32>,
    seen: Vec<u8>,
    delivered: u64,
    timeouts: u64,
    lost: u64,
    late: u64,
    aborted: bool,
    csw: i64,
}

struct WakerOut {
    cost: Vec<u32>,
    parked: Vec<bool>,
    writes: u64,
    csw: i64,
}

/// Raises `stop` when its thread unwinds, so the other threads of a run do not wait forever.
struct StopOnPanic<'a>(&'a AtomicBool);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.store(true, Relaxed);
        }
    }
}

fn shard_run<P: Protocol>(wk: &Wake<u64>, ctl: &WakeCtl, cfg: &WakeCfg) -> ShardOut {
    let _guard = StopOnPanic(&ctl.stop);
    if let Some(c) = &cfg.cpus {
        pin(c[0]);
    }
    let (n, total) = (cfg.per_waker as usize, cfg.wakers as u64 * cfg.per_waker);
    let mut out = ShardOut {
        park: vec![],
        lat: vec![0; cfg.wakers * n],
        seen: vec![0; cfg.wakers * n],
        delivered: 0,
        timeouts: 0,
        lost: 0,
        late: 0,
        aborted: false,
        csw: 0,
    };
    let mut parker = Parker {
        cost: Vec::with_capacity(total as usize * 2),
        slept_at: 0,
    };
    let mut last = vec![0u64; cfg.wakers];
    let mut cnt = vec![0u64; cfg.wakers];
    loop {
        while let Some(m) = wk.inbox.pop() {
            let (ts, w, seq) = unpack(m);
            let idx = w * n + seq as usize;
            out.lat[idx] = ctl.clock.ns().saturating_sub(ts).min(u64::from(u32::MAX)) as u32;
            out.seen[idx] = out.seen[idx].saturating_add(1);
            out.late += u64::from(ts + LATE_NS < parker.slept_at);
            cnt[w] += 1;
            out.delivered += 1;
        }
        for ((a, l), c) in ctl.acked.iter().zip(&mut last).zip(&cnt) {
            if l != c {
                a.store(*c, Release);
                *l = *c;
            }
        }
        if out.delivered >= total || ctl.stop.load(Relaxed) {
            break;
        }
        if let Park::TimedOut { lost } = wk.park::<P>(&mut parker, 100, ctl.clock) {
            out.timeouts += 1;
            out.lost += u64::from(lost);
            if out.lost >= cfg.lost_abort {
                out.aborted = true;
                ctl.stop.store(true, Relaxed);
            }
        }
        assert!(
            ctl.clock.ns() < RUN_LIMIT_NS,
            "wake run exceeded its time limit"
        );
    }
    out.park = parker.cost;
    out.csw = involuntary_csw();
    out
}

fn waker_run<P: Protocol>(wk: &Wake<u64>, ctl: &WakeCtl, cfg: &WakeCfg, wi: usize) -> WakerOut {
    let _guard = StopOnPanic(&ctl.stop);
    if let Some(c) = &cfg.cpus {
        pin(c[1 + wi]);
    }
    let n = cfg.per_waker;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (wi as u64 + 1).wrapping_mul(0xD1B5_4A32_D192_ED03));
    let mut out = WakerOut {
        cost: Vec::with_capacity(n as usize),
        parked: Vec::with_capacity(n as usize),
        writes: 0,
        csw: 0,
    };
    let mut seq = 0;
    while seq < n && !ctl.stop.load(Relaxed) {
        for _ in 0..(1 + rng.next() % BURST_MAX).min(n - seq) {
            let t0 = ctl.clock.ns();
            let was_parked = wk.send::<P>(pack(t0, wi, seq));
            out.cost.push((ctl.clock.ns() - t0) as u32);
            out.parked.push(was_parked);
            out.writes += u64::from(was_parked);
            seq += 1;
        }
        spin_until("shard ack", || {
            ctl.acked[wi].load(Acquire) >= seq || ctl.stop.load(Relaxed)
        });
        // Three bursts in eight follow the ack within `FINE_NS`, sweeping the nanosecond window in
        // which the shard publishes `parked` and re-checks the inbox; the rest wait a random gap.
        let gap = if rng.next() % 8 < 3 {
            rng.next() % FINE_NS
        } else {
            rng.next() % (2 * cfg.gap_ns)
        };
        spin_for(ctl.clock, gap);
    }
    out.csw = involuntary_csw();
    out
}

// Column indices of a wake row.
const MSGS: usize = 0;
const LOST: usize = 12;
const ABORT: usize = 14;
const ERR: usize = 16;

fn summarize_wake(cfg: &WakeCfg, s: &ShardOut, ws: &[WakerOut]) -> Vec<f64> {
    let per = cfg.per_waker as usize;
    let (mut all, mut fast, mut sig, mut lat) = (vec![], vec![], vec![], vec![]);
    for (wi, w) in ws.iter().enumerate() {
        for (seq, (&c, &p)) in w.cost.iter().zip(&w.parked).enumerate() {
            all.push(c);
            if !p {
                fast.push(c);
            } else {
                sig.push(c);
                if s.seen[wi * per + seq] == 1 {
                    lat.push(s.lat[wi * per + seq]);
                }
            }
        }
    }
    let sent = all.len() as f64;
    let writes: u64 = ws.iter().map(|w| w.writes).sum();
    let dups = s.seen.iter().filter(|&&c| c > 1).count();
    let missing = s.seen.iter().filter(|&&c| c == 0).count();
    let errors = dups + if s.aborted { 0 } else { missing };
    let (all, fast, sig, lat, park) = (
        sorted(all),
        sorted(fast),
        sorted(sig),
        sorted(lat),
        sorted(s.park.clone()),
    );
    let csw = s.csw + ws.iter().map(|w| w.csw).sum::<i64>();
    vec![
        s.delivered as f64,
        quant(&all, 0.5),
        quant(&all, 0.99),
        quant(&fast, 0.5),
        quant(&sig, 0.5),
        100.0 * sig.len() as f64 / sent,
        quant(&lat, 0.5),
        quant(&lat, 0.99),
        quant(&lat, 0.999),
        writes as f64 / sent,
        quant(&park, 0.5),
        park.len() as f64,
        s.lost as f64,
        s.timeouts as f64,
        f64::from(u8::from(s.aborted)),
        csw as f64,
        errors as f64,
        s.late as f64,
    ]
}

fn run_wake<P: Protocol>(cfg: &WakeCfg) -> Vec<f64> {
    P::prepare();
    assert!(
        cfg.per_waker <= SEQ_MASK,
        "per-waker count overflows the message sequence"
    );
    let wk = Wake::<u64>::new();
    let ctl = WakeCtl {
        clock: Clock::new(),
        acked: (0..cfg.wakers)
            .map(|_| CachePadded::new(AtomicU64::new(0)))
            .collect(),
        stop: AtomicBool::new(false),
    };
    let (s, ws) = thread::scope(|sc| {
        let (wk, ctl) = (&wk, &ctl);
        let shard = sc.spawn(move || shard_run::<P>(wk, ctl, cfg));
        let wakers: Vec<_> = (0..cfg.wakers)
            .map(|wi| sc.spawn(move || waker_run::<P>(wk, ctl, cfg, wi)))
            .collect();
        let ws: Vec<WakerOut> = wakers
            .into_iter()
            .map(|h| h.join().expect("waker"))
            .collect();
        (shard.join().expect("shard"), ws)
    });
    summarize_wake(cfg, &s, &ws)
}

fn variant<P: Protocol>(cfg: &WakeCfg) -> (&'static str, Vec<Vec<f64>>) {
    (P::NAME, (0..RUNS).map(|_| run_wake::<P>(cfg)).collect())
}

/// Single-thread, uncontended fast paths: the waker's push + fence + load (+ pop outside the
/// timed span) and the shard's publish + fence + re-check. Medians in ns of `MICRO_ITERS` reps.
fn micro<P: Protocol>(clock: Clock) -> Vec<f64> {
    P::prepare();
    let wk = Wake::<u64>::new();
    let (mut waker, mut shard) = (
        Vec::with_capacity(MICRO_ITERS),
        Vec::with_capacity(MICRO_ITERS),
    );
    for i in 0..MICRO_ITERS {
        let t0 = clock.ns();
        wk.inbox.push(i as u64);
        P::waker_fence();
        let parked = wk.parked.load(Relaxed);
        waker.push((clock.ns() - t0) as u32);
        assert!(!parked && wk.inbox.pop().is_some());
        let t0 = clock.ns();
        let empty = wk.arm::<P>();
        shard.push((clock.ns() - t0) as u32);
        assert!(empty);
        wk.parked.store(false, Relaxed);
    }
    vec![quant(&sorted(waker), 0.5), quant(&sorted(shard), 0.5)]
}

/// Two-thread spin barrier: `n` must increase by one per call on both threads.
fn rendezvous(flags: &[CachePadded<AtomicU64>; 2], me: usize, n: u64) {
    flags[me].store(n, Release);
    spin_until("litmus peer", || flags[1 - me].load(Acquire) >= n);
}

/// Races two halves of a Dekker pair for `rounds` rounds, each half starting after a small random
/// skew behind a spin barrier. `shard` and `waker` return whether their half saw nothing; a round
/// is lost when both did (the shard sleeps on a message the waker never signals). Returns the lost
/// rounds. `reset` runs between rounds. This stresses the nanosecond window that the throughput
/// workload only reaches by chance.
fn litmus(
    rounds: u64,
    cpus: Option<&[usize]>,
    shard: impl Fn() -> bool + Sync,
    waker: impl Fn() -> bool + Sync,
    reset: impl Fn() + Sync,
) -> u64 {
    let flags = [0, 1].map(|_| CachePadded::new(AtomicU64::new(0)));
    let saw_nothing = AtomicBool::new(false);
    let skew = |rng: &mut Rng| {
        for _ in 0..rng.next() % LITMUS_SKEW {
            std::hint::black_box(());
        }
    };
    thread::scope(|sc| {
        let (flags, saw_nothing) = (&flags, &saw_nothing);
        let (shard, waker, reset) = (&shard, &waker, &reset);
        let w = sc.spawn(move || {
            cpus.inspect(|c| pin(c[1]));
            let mut rng = Rng(0xD1B5_4A32_D192_ED03);
            for r in 0..rounds {
                rendezvous(flags, 1, 3 * r + 1);
                skew(&mut rng);
                saw_nothing.store(waker(), Relaxed);
                rendezvous(flags, 1, 3 * r + 2);
                rendezvous(flags, 1, 3 * r + 3);
            }
        });
        let s = sc.spawn(move || {
            cpus.inspect(|c| pin(c[0]));
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            let mut lost = 0;
            for r in 0..rounds {
                rendezvous(flags, 0, 3 * r + 1);
                skew(&mut rng);
                let empty = shard();
                rendezvous(flags, 0, 3 * r + 2);
                lost += u64::from(empty && saw_nothing.load(Relaxed));
                reset();
                rendezvous(flags, 0, 3 * r + 3);
            }
            lost
        });
        w.join().expect("litmus waker");
        s.join().expect("litmus shard")
    })
}

/// The real park/wake pair (`Wake::arm` against `Wake::send` minus the eventfd) under `litmus`.
fn litmus_wake<P: Protocol>(rounds: u64, cpus: Option<&[usize]>) -> u64 {
    P::prepare();
    let wk = Wake::<u64>::new();
    litmus(
        rounds,
        cpus,
        || {
            wk.parked.store(true, Relaxed);
            P::shard_fence();
            wk.inbox.is_empty()
        },
        || {
            wk.inbox.push(1);
            P::waker_fence();
            !wk.parked.load(Relaxed)
        },
        || {
            while wk.inbox.pop().is_some() {}
            wk.parked.store(false, Relaxed);
        },
    )
}

/// Classic store-buffering test on two plain atomics; `fenced` puts `fence(SeqCst)` on both sides.
/// Calibrates the harness: unfenced, x86 must show the store-load reordering.
fn litmus_plain(rounds: u64, cpus: Option<&[usize]>, fenced: bool) -> u64 {
    let (x, y) = (
        CachePadded::new(AtomicU64::new(0)),
        CachePadded::new(AtomicU64::new(0)),
    );
    let barrier = |f: bool| {
        if f {
            fence(SeqCst);
        }
    };
    litmus(
        rounds,
        cpus,
        || {
            x.store(1, Relaxed);
            barrier(fenced);
            y.load(Relaxed) == 0
        },
        || {
            y.store(1, Relaxed);
            barrier(fenced);
            x.load(Relaxed) == 0
        },
        || {
            x.store(0, Relaxed);
            y.store(0, Relaxed);
        },
    )
}

#[test]
#[ignore]
fn bench_wake_handshake() {
    let _exclusive = exclusive();
    let clock = Clock::new();
    let reg = membarrier_register();
    for w in [1usize, 4, 16] {
        let cpus = pick_cpus(w + 1);
        banner(
            &format!("wake fence, {w} waker(s), shard on cpu {}", cpus[0]),
            &cpus,
        );
        println!(
            "timer floor (ns, one Clock::ns pair): {}",
            num(timer_floor(clock))
        );
        match reg {
            Ok(ns) => println!(
                "membarrier PRIVATE_EXPEDITED registration (first call in process): {ns} ns"
            ),
            Err(e) => println!("membarrier registration FAILED: {e}"),
        }
        let cfg = WakeCfg {
            wakers: w,
            per_waker: 200_000 / w as u64,
            cpus: Some(cpus),
            gap_ns: 20_000,
            lost_abort: 10,
        };
        let results = [
            variant::<SeqCstBoth>(&cfg),
            variant::<Membarrier>(&cfg),
            variant::<NoFence>(&cfg),
        ];
        let rows: Vec<Row> = results
            .iter()
            .map(|(n, runs)| ((*n).into(), median_cols(runs)))
            .collect();
        let spread: Vec<Row> = results
            .iter()
            .flat_map(|(n, runs)| spread_rows(n.split(' ').next().unwrap_or(n), runs))
            .collect();
        let t = format!("waker side, {w} waker(s), median of {RUNS} runs (ns unless noted)");
        table(
            &t,
            &[
                ("msgs", 0),
                ("cost_med", 1),
                ("cost_p99", 2),
                ("nosig_med", 3),
                ("sig_med", 4),
                ("%parked", 5),
                ("ev_wr/msg", 9),
            ],
            &rows,
        );
        let t = format!("wake side, {w} waker(s), median of {RUNS} runs (ns unless noted)");
        table(
            &t,
            &[
                ("lat_p50", 6),
                ("lat_p99", 7),
                ("lat_p999", 8),
                ("park_med", 10),
                ("parks", 11),
                ("lost", 12),
                ("timeouts", 13),
                ("late", 17),
                ("aborted", 14),
                ("inv_csw", 15),
                ("errors", 16),
            ],
            &rows,
        );
        let t = format!("spread over {RUNS} runs, {w} waker(s) (ns)");
        table(
            &t,
            &[
                ("cost_med", 1),
                ("lat_p50", 6),
                ("lat_p99", 7),
                ("park_med", 10),
                ("lost", 12),
            ],
            &spread,
        );
        println!(
            "loadavg after: {}",
            std::fs::read_to_string("/proc/loadavg")
                .unwrap_or_default()
                .trim()
        );
    }
    let cpus = pick_cpus(1);
    banner("fast-path micro (single thread, no contention)", &cpus);
    // Pinned on a scoped thread so this test thread keeps its CPU set.
    let rows: Vec<Row> = thread::scope(|sc| {
        sc.spawn(|| {
            pin(cpus[0]);
            vec![
                ("A fence(SeqCst) both".into(), micro::<SeqCstBoth>(clock)),
                ("B membarrier shard".into(), micro::<Membarrier>(clock)),
                ("C no hw fence (control)".into(), micro::<NoFence>(clock)),
            ]
        })
        .join()
        .expect("micro")
    });
    table(
        &format!(
            "micro, median of {MICRO_ITERS} reps (ns, includes the {} ns timer floor)",
            num(timer_floor(clock))
        ),
        &[("waker_path", 0), ("shard_arm", 1)],
        &rows,
    );
    let cpus = pick_cpus(2);
    banner("litmus: tight race of the park/wake pair", &cpus);
    let c = Some(&cpus[..]);
    type Case<'a> = (&'a str, Box<dyn Fn() -> u64 + 'a>);
    let cases: [Case<'_>; 5] = [
        (
            "A fence(SeqCst) both",
            Box::new(move || litmus_wake::<SeqCstBoth>(LITMUS_ROUNDS, c)),
        ),
        (
            "B membarrier shard",
            Box::new(move || litmus_wake::<Membarrier>(LITMUS_ROUNDS, c)),
        ),
        (
            "C no hw fence (control)",
            Box::new(move || litmus_wake::<NoFence>(LITMUS_ROUNDS, c)),
        ),
        (
            "plain SB, no fences",
            Box::new(move || litmus_plain(LITMUS_ROUNDS, c, false)),
        ),
        (
            "plain SB, fence both",
            Box::new(move || litmus_plain(LITMUS_ROUNDS, c, true)),
        ),
    ];
    let (mut rows, mut spread) = (vec![], vec![]);
    for (name, run) in &cases {
        let runs: Vec<Vec<f64>> = (0..RUNS)
            .map(|_| vec![LITMUS_ROUNDS as f64, run() as f64])
            .collect();
        rows.push(((*name).to_owned(), median_cols(&runs)));
        spread.extend(spread_rows(name, &runs));
    }
    table(
        &format!(
            "litmus, {LITMUS_ROUNDS} rounds per run, lost = sleeper saw an empty inbox and the waker saw parked=false; median of {RUNS} runs"
        ),
        &[("rounds", 0), ("lost", 1)],
        &rows,
    );
    table("litmus spread over runs", &[("lost", 1)], &spread);
}

fn exactly_once<P: Protocol>() {
    let cfg = WakeCfg {
        wakers: 2,
        per_waker: 10_000,
        cpus: None,
        gap_ns: 2_000,
        lost_abort: 1,
    };
    let r = run_wake::<P>(&cfg);
    assert_eq!(
        r[MSGS],
        20_000.0,
        "{}: not every message was delivered",
        P::NAME
    );
    assert_eq!(r[LOST], 0.0, "{}: lost wakeup", P::NAME);
    assert_eq!(r[ABORT], 0.0, "{}: run aborted", P::NAME);
    assert_eq!(r[ERR], 0.0, "{}: duplicated or missing messages", P::NAME);
}

#[test]
fn wake_fence_seqcst_delivers_exactly_once() {
    exactly_once::<SeqCstBoth>();
}

#[test]
fn wake_fence_membarrier_delivers_exactly_once() {
    exactly_once::<Membarrier>();
}

#[test]
fn wake_detector_fires_on_unsignalled_push() {
    let (wk, clock, mut parker) = (Wake::<u64>::new(), Clock::new(), Parker::default());
    // A waker that read `parked == false` pushes without signalling while the shard sleeps.
    assert!(wk.arm::<SeqCstBoth>());
    wk.inbox.push(pack(0, 0, 0));
    assert_eq!(
        wk.wait(&mut [EMPTY_EV; 1], 20),
        0,
        "no eventfd write, so the wait must time out"
    );
    assert!(
        !wk.inbox.is_empty(),
        "timed out with a message queued: the lost-wakeup condition"
    );
    // An idle timeout with an empty inbox is not a lost wakeup.
    assert!(wk.inbox.pop().is_some());
    assert!(matches!(
        wk.park::<SeqCstBoth>(&mut parker, 20, clock),
        Park::TimedOut { lost: false }
    ));
    // A signalled send wakes the sleeping shard.
    assert!(wk.arm::<SeqCstBoth>());
    assert!(wk.send::<SeqCstBoth>(pack(0, 0, 1)));
    assert!(wk.wait(&mut [EMPTY_EV; 1], 1000) > 0);
}

#[test]
fn wake_litmus_fenced_variants_never_lose() {
    assert_eq!(litmus_wake::<SeqCstBoth>(20_000, None), 0);
    assert_eq!(litmus_wake::<Membarrier>(20_000, None), 0);
    assert_eq!(litmus_plain(20_000, None, true), 0);
}
