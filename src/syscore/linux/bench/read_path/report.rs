//! Aggregation over runs, the tables and the threshold rule.

use super::*;

/// Median and range of a set of runs.
#[derive(Clone, Copy, Debug)]
pub struct Agg {
    pub med: f64,
    pub min: f64,
    pub max: f64,
}

impl Agg {
    pub fn of(it: impl IntoIterator<Item = f64>) -> Self {
        let mut v: Vec<f64> = it.into_iter().collect();
        v.sort_by(f64::total_cmp);
        let n = v.len();
        Self {
            med: (v[(n - 1) / 2] + v[n / 2]) / 2.0,
            min: v[0],
            max: v[n - 1],
        }
    }

    pub fn spread(&self) -> f64 {
        (self.max - self.min) / self.med
    }

    pub fn show(&self, prec: usize) -> String {
        format!(
            "{:.prec$} [{:.prec$}..{:.prec$}]",
            self.med, self.min, self.max
        )
    }

    /// Whether the two ranges share a value, so a ratio of medians is not clearly a win.
    pub fn overlaps(&self, o: &Agg) -> bool {
        self.min <= o.max && o.min <= self.max
    }
}

/// Median over runs of a per-run rate `num / den`.
pub fn rate<R>(runs: &[R], num: impl Fn(&R) -> f64, den: impl Fn(&R) -> f64) -> f64 {
    Agg::of(runs.iter().map(|r| num(r) / den(r))).med
}

pub struct StreamPoint {
    pub gbps: Agg,
    pub cpu_ns_kib: Agg,
    pub enter: f64,
    pub recv: f64,
    pub epoll: f64,
    pub kib_per_read: f64,
    pub enobufs: u64,
    /// Share of the wall time the reader thread spent on a CPU.
    pub busy: f64,
}

impl StreamPoint {
    pub fn build(runs: &[StreamRun]) -> Self {
        let (reads, bytes) = (
            |r: &StreamRun| r.c.reads as f64,
            |r: &StreamRun| r.c.bytes as f64,
        );
        Self {
            gbps: Agg::of(runs.iter().map(|r| bytes(r) / r.wall_s / 1e9)),
            cpu_ns_kib: Agg::of(runs.iter().map(|r| r.cpu_ns as f64 / (bytes(r) / 1024.0))),
            enter: rate(runs, |r| r.c.enter as f64, reads),
            recv: rate(runs, |r| r.c.recv as f64, reads),
            epoll: rate(runs, |r| r.c.epoll_wait as f64, reads),
            kib_per_read: rate(runs, |r| bytes(r) / 1024.0, reads),
            enobufs: runs.iter().map(|r| r.c.enobufs).sum(),
            busy: rate(runs, |r| r.cpu_ns as f64 / 1e9, |r| r.wall_s),
        }
    }
}

pub struct PpPoint {
    pub p50: Agg,
    pub p99: Agg,
    pub cpu_us_rt: f64,
    pub enter: f64,
    pub recv: f64,
    pub epoll: f64,
    pub send: f64,
    pub rts: u64,
    pub enobufs: u64,
}

impl PpPoint {
    pub fn build(runs: &[PpRun]) -> Self {
        let rts = |r: &PpRun| r.rts as f64;
        Self {
            p50: Agg::of(runs.iter().map(|r| r.p50_us)),
            p99: Agg::of(runs.iter().map(|r| r.p99_us)),
            cpu_us_rt: Agg::of(runs.iter().map(|r| r.cpu_us_rt)).med,
            enter: rate(runs, |r| r.c.enter as f64, rts),
            recv: rate(runs, |r| r.c.recv as f64, rts),
            epoll: rate(runs, |r| r.c.epoll_wait as f64, rts),
            send: rate(runs, |r| r.c.send as f64, rts),
            rts: runs.iter().map(|r| r.rts).min().unwrap_or(0),
            enobufs: runs.iter().map(|r| r.c.enobufs).sum(),
        }
    }
}

/// The three paths' points for one size, and how many attempts it took to get a quiet one.
pub struct Row<P> {
    pub s: usize,
    pub pts: [P; 3],
    pub attempts: usize,
}

impl<P> std::ops::Index<Path> for Row<P> {
    type Output = P;

    fn index(&self, p: Path) -> &P {
        &self.pts[p as usize]
    }
}

/// Runs `RUNS` rounds, each round one run per path (interleaved, so a load change hits all three
/// alike), and reruns the whole point while any path's range exceeds `SPREAD_LIMIT`. Per path it
/// keeps the attempt with the smallest spread, which a table prints as `tries`.
pub fn measure_point<R, P>(
    one: impl Fn(Path) -> io::Result<R>,
    build: impl Fn(&[R]) -> P,
    spread: impl Fn(&P) -> f64,
) -> io::Result<([P; 3], usize)> {
    let mut best: [Option<P>; 3] = [None, None, None];
    let mut attempts = 0;
    while attempts < MAX_ATTEMPTS {
        attempts += 1;
        let mut runs: [Vec<R>; 3] = Default::default();
        for _ in 0..RUNS {
            for p in Path::ALL {
                runs[p as usize].push(one(p)?);
            }
        }
        let mut quiet = true;
        for p in Path::ALL {
            let pt = build(&runs[p as usize]);
            let sp = spread(&pt);
            quiet &= sp <= SPREAD_LIMIT;
            if best[p as usize].as_ref().is_none_or(|b| sp < spread(b)) {
                best[p as usize] = Some(pt);
            }
        }
        if quiet {
            break;
        }
    }
    Ok((best.map(|b| b.expect("an attempt ran")), attempts))
}

/// The L3 size and where it came from; 64 MiB, said so, when the machine will not tell.
pub fn l3_bytes() -> (usize, &'static str) {
    let getconf = Command::new("getconf")
        .arg("LEVEL3_CACHE_SIZE")
        .output()
        .ok();
    let getconf = getconf.and_then(|o| String::from_utf8(o.stdout).ok()?.trim().parse().ok());
    if let Some(b) = getconf.filter(|&b| b > 0) {
        return (b, "getconf LEVEL3_CACHE_SIZE");
    }
    let kib = fs::read_to_string("/sys/devices/system/cpu/cpu0/cache/index3/size")
        .ok()
        .and_then(|t| t.trim().strip_suffix('K')?.parse::<usize>().ok());
    match kib {
        Some(k) => (k << 10, "sysfs cache/index3/size"),
        None => (
            64 << 20,
            "FALLBACK 64 MiB: neither getconf nor sysfs gave an L3 size",
        ),
    }
}

pub fn human(b: usize) -> String {
    match b {
        b if b >= 1 << 20 => format!("{} MiB", b >> 20),
        b if b >= 1 << 10 => format!("{} KiB", b >> 10),
        b => format!("{b} B"),
    }
}

/// Three significant digits, so a rate of 0.0004 per read does not print as 0.
pub fn sig(x: f64) -> String {
    let decimals = (2 - x.abs().log10().floor() as i32).clamp(0, 6);
    format!("{x:.*}", decimals as usize)
}

pub fn note(spread: f64, extra: String) -> String {
    let noisy = (spread > SPREAD_LIMIT).then(|| format!("NOISY {:.0}%", spread * 100.0));
    [noisy.unwrap_or_default(), extra]
        .join(" ")
        .trim()
        .to_string()
}

/// Prints a table, every column as wide as its widest cell.
pub fn table(head: &[&str], rows: &[Vec<String>]) {
    let w: Vec<usize> = (0..head.len())
        .map(|i| {
            let widest = rows.iter().map(|r| r[i].len()).max().unwrap_or(0);
            widest.max(head[i].len())
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let cells: Vec<String> = cells
            .iter()
            .zip(&w)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", cells.join("  ").trim_end());
    };
    line(head.to_vec());
    rows.iter()
        .for_each(|r| line(r.iter().map(String::as_str).collect()));
}

pub type Pools = BTreeMap<usize, PoolCfg>;
pub type StreamRow = Row<StreamPoint>;

pub fn measure_stream(
    title: &str,
    cold: Option<usize>,
    cpus: Cpus,
    pools: &Pools,
) -> io::Result<Vec<StreamRow>> {
    let lim = StreamLimits {
        bytes: STREAM_BYTES,
        secs: STREAM_SECS,
    };
    let mut rows = Vec::new();
    for s in SIZES {
        let (pts, attempts) = measure_point(
            |p| stream_run(p, s, cold, pools[&s], &lim, Some(cpus)),
            StreamPoint::build,
            |p| p.gbps.spread(),
        )?;
        println!("  [{title} S={}] {attempts} attempt(s)", human(s));
        rows.push(Row { s, pts, attempts });
    }
    println!("\n== streaming, {title} destination (median [min..max] of {RUNS} runs) ==");
    let head = [
        "S",
        "path",
        "GB/s",
        "cpu ns/KiB",
        "busy",
        "enter/rd",
        "recv/rd",
        "epoll/rd",
        "KiB/rd",
        "tries",
        "note",
    ];
    let cells = rows.iter().flat_map(|row| {
        Path::ALL.map(|p| {
            let pt = &row[p];
            let extra = if p == Path::Pool {
                format!("enobufs={}", pt.enobufs)
            } else {
                String::new()
            };
            vec![
                human(row.s),
                p.tag().into(),
                pt.gbps.show(3),
                pt.cpu_ns_kib.show(1),
                format!("{:.0}%", pt.busy * 100.0),
                sig(pt.enter),
                sig(pt.recv),
                sig(pt.epoll),
                sig(pt.kib_per_read),
                row.attempts.to_string(),
                note(pt.gbps.spread(), extra),
            ]
        })
    });
    table(&head, &cells.collect::<Vec<_>>());
    Ok(rows)
}

pub fn measure_pingpong(cpus: Cpus, pools: &Pools) -> io::Result<Vec<Row<PpPoint>>> {
    let mut rows = Vec::new();
    for s in PP_SIZES {
        let (pts, attempts) = measure_point(
            |p| pingpong_run(p, s, PP_ROUND_TRIPS, pools[&s], Some(cpus), false),
            PpPoint::build,
            |p| p.p50.spread(),
        )?;
        println!("  [pingpong S={}] {attempts} attempt(s)", human(s));
        rows.push(Row { s, pts, attempts });
    }
    println!(
        "\n== ping-pong, S bytes each way, us (median [min..max] of {RUNS} runs; '+' = size added for the rule) =="
    );
    let head = [
        "S",
        "path",
        "p50",
        "p99",
        "cpu us/rt",
        "enter/rt",
        "recv/rt",
        "epoll/rt",
        "send/rt",
        "rts/run",
        "tries",
        "note",
    ];
    let cells = rows.iter().flat_map(|row| {
        let mark = if [1 << 10, 16 << 10, 64 << 10].contains(&row.s) {
            ""
        } else {
            "+"
        };
        Path::ALL.map(|p| {
            let pt = &row[p];
            vec![
                format!("{}{mark}", human(row.s)),
                p.tag().into(),
                pt.p50.show(2),
                pt.p99.show(2),
                sig(pt.cpu_us_rt),
                sig(pt.enter),
                sig(pt.recv),
                sig(pt.epoll),
                sig(pt.send),
                pt.rts.to_string(),
                row.attempts.to_string(),
                note(
                    pt.p50.spread(),
                    if p == Path::Pool {
                        format!("enobufs={}", pt.enobufs)
                    } else {
                        String::new()
                    },
                ),
            ]
        })
    });
    table(&head, &cells.collect::<Vec<_>>());
    Ok(rows)
}

/// Applies the threshold rule to `cand` against path A, prints its inputs and returns the
/// smallest qualifying size.
pub fn decide(
    cand: Path,
    hot: &[StreamRow],
    cold: &[StreamRow],
    pp: &[Row<PpPoint>],
) -> Option<usize> {
    println!(
        "\n-- rule for path {}: hot AND cold GB/s > {WIN} x A, and ping-pong p50 <= {WIN} x A --",
        cand.tag()
    );
    let head = [
        "S",
        "hot",
        "cold",
        "pp p50",
        "cpu hot",
        "cpu cold",
        "hot ok",
        "cold ok",
        "pp ok",
        "qualifies",
    ];
    let (mut cells, mut first) = (Vec::new(), None);
    for ((h, c), s) in hot.iter().zip(cold).zip(SIZES) {
        let Some(p) = pp.iter().find(|r| r.s == s) else {
            continue;
        };
        // A '~' marks a ratio whose min..max range overlaps A's: not clearly a win or a loss.
        let ratio = |a: &Agg, b: &Agg| {
            let mark = if a.overlaps(b) { "~" } else { "" };
            (b.med / a.med, mark)
        };
        let stream = |x: &StreamRow| ratio(&x[Path::Pool].gbps, &x[cand].gbps);
        let ((hr, hm), (cr, cm)) = (stream(h), stream(c));
        let (pr, pm) = ratio(&p[Path::Pool].p50, &p[cand].p50);
        let ok = [hr > WIN, cr > WIN, pr <= WIN];
        let all = ok.iter().all(|&b| b);
        first = first.or(all.then_some(s));
        cells.push(vec![
            human(s),
            format!("{hr:.3}{hm}"),
            format!("{cr:.3}{cm}"),
            format!("{pr:.3}{pm}"),
            format!(
                "{:.3}",
                h[cand].cpu_ns_kib.med / h[Path::Pool].cpu_ns_kib.med
            ),
            format!(
                "{:.3}",
                c[cand].cpu_ns_kib.med / c[Path::Pool].cpu_ns_kib.med
            ),
            ok[0].to_string(),
            ok[1].to_string(),
            ok[2].to_string(),
            all.to_string(),
        ]);
    }
    table(&head, &cells);
    println!(
        "(hot, cold, pp p50 and the two cpu columns are {} / A; the cpu columns are reader CPU per KiB and are not part of the rule; '~' = min..max ranges overlap A's)",
        cand.tag()
    );
    first
}

pub fn verdict(hot: &[StreamRow], cold: &[StreamRow], pp: &[Row<PpPoint>]) {
    for (cand, what) in [
        (
            Path::UringPoll,
            "PROPOSED THRESHOLD (candidate B; the runtime is on io_uring)",
        ),
        (Path::Epoll, "note, path C on the same rule"),
    ] {
        match decide(cand, hot, cold, pp) {
            Some(s) => println!("{what}: reads of {} and above bypass the pool", human(s)),
            None => println!("{what}: no threshold: always use the pool"),
        }
    }
}
