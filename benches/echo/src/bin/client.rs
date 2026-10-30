//! The load generator every server is measured with: one pinned thread per CPU of
//! `cpus=`, each driving its share of the connections from a plain epoll loop, so
//! no runtime under comparison sits in the client.
//!
//! `load=closed` sends a connection's next request when its response arrives: the
//! latency is the round trip and the rate is the saturation throughput.
//!
//! `load=steady rate=R` and `load=burst low=L high=H period_ms=P` are open loop.
//! Each connection follows its own schedule of intended send times (its share of
//! the rate, constant or alternating between `L` and `H` every period, from a
//! random phase), and a request's latency counts from its intended time: a stall
//! delays every request scheduled behind it, and none is left out (no coordinated
//! omission). A request scheduled in the window that has not completed when the
//! drain deadline passes counts as unfinished, at the latency it had reached.
//! `lag` is how late the client itself sent requests whose connection was idle;
//! a large lag means the client, not the server, set the latency.
//!
//! Usage: `client port=P conns=C req=B resp=B load=closed|steady|burst [rate=R]
//! [low=L high=H period_ms=P] warm=S dur=S pid=PID cpus=a,b,.. [spin=0|1]`

use echo::{RESPONSE_BYTE, pin, raise_nofile};
use hdrhistogram::Histogram;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DRAIN: Duration = Duration::from_secs(2);
const SPIN_WITHIN: Duration = Duration::from_micros(100);
const CHECK_EVERY: u64 = 64;
const MAX_NS: u64 = 60_000_000_000;

#[derive(Clone, Copy)]
enum Load {
    Closed,
    Steady {
        rate: f64,
    },
    Burst {
        low: f64,
        high: f64,
        period: Duration,
    },
}

impl Load {
    fn rate_at(self, t0: Instant, t: Instant) -> f64 {
        match self {
            Load::Closed => f64::INFINITY,
            Load::Steady { rate } => rate,
            Load::Burst { low, high, period } => {
                let k = t.saturating_duration_since(t0).as_nanos() / period.as_nanos().max(1);
                if k.is_multiple_of(2) { low } else { high }
            }
        }
    }
}

#[derive(Clone)]
struct Opts {
    port: u16,
    conns: usize,
    req: usize,
    resp: usize,
    load: Load,
    warm: Duration,
    dur: Duration,
    pid: u32,
    cpus: Vec<usize>,
    spin: bool,
}

fn parse() -> Result<Opts, String> {
    let args: Vec<(String, String)> = std::env::args()
        .skip(1)
        .map(|a| {
            a.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| format!("argument {a} is not key=value"))
        })
        .collect::<Result<_, _>>()?;
    let get = |k: &str| {
        args.iter()
            .rev()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let num = |k: &str| -> Result<f64, String> {
        let v = get(k).ok_or_else(|| format!("missing {k}="))?;
        v.parse::<f64>().map_err(|e| format!("{k}={v}: {e}"))
    };
    let positive = |k: &str| -> Result<f64, String> {
        let v = num(k)?;
        if v > 0.0 && v.is_finite() {
            Ok(v)
        } else {
            Err(format!("{k} must be positive"))
        }
    };
    let load = match get("load").unwrap_or("closed") {
        "closed" => Load::Closed,
        "steady" => Load::Steady {
            rate: positive("rate")?,
        },
        "burst" => Load::Burst {
            low: positive("low")?,
            high: positive("high")?,
            period: Duration::from_secs_f64(positive("period_ms")? / 1000.0),
        },
        other => return Err(format!("load={other}: closed, steady or burst")),
    };
    let cpus: Vec<usize> = get("cpus")
        .ok_or("missing cpus=")?
        .split(',')
        .map(|c| c.parse().map_err(|e| format!("cpus: {e}")))
        .collect::<Result<_, _>>()?;
    Ok(Opts {
        port: num("port")? as u16,
        conns: positive("conns")? as usize,
        req: positive("req")? as usize,
        resp: positive("resp")? as usize,
        load,
        warm: Duration::from_secs_f64(num("warm")?.max(0.0)),
        dur: Duration::from_secs_f64(positive("dur")?),
        pid: num("pid").unwrap_or(0.0) as u32,
        cpus,
        spin: get("spin").is_none_or(|v| v != "0"),
    })
}

fn cpu_ticks(pid: u32) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let rest = s.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    let at = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    at(11) + at(12)
}

fn nanos(d: Duration) -> u64 {
    (d.as_nanos() as u64).clamp(1, MAX_NS)
}

fn unit(rng: &mut u64) -> f64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    (*rng >> 11) as f64 / (1u64 << 53) as f64
}

fn histogram() -> Histogram<u64> {
    Histogram::new_with_max(MAX_NS, 3).expect("the bounds are valid")
}

struct Tally {
    hist: Histogram<u64>,
    lag: Histogram<u64>,
    completed: u64,
    unfinished: u64,
}

struct Conn {
    sock: TcpStream,
    due: Instant,
    intended: Instant,
    sent: usize,
    got: usize,
    busy: bool,
    late: bool,
    done: bool,
}

struct Epoll(i32);

impl Epoll {
    fn new() -> io::Result<Epoll> {
        // SAFETY: a plain syscall; the descriptor is owned by the returned value.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Epoll(fd))
        }
    }

    fn add(&self, fd: i32, token: usize) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLET) as u32,
            u64: token as u64,
        };
        // SAFETY: both descriptors are open and `ev` outlives the call.
        if unsafe { libc::epoll_ctl(self.0, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn wait(&self, events: &mut [libc::epoll_event], timeout: Duration) -> io::Result<usize> {
        let ts = libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };
        // SAFETY: `events` is writable for its length and `ts` outlives the call.
        let n = unsafe {
            libc::epoll_pwait2(
                self.0,
                events.as_mut_ptr(),
                events.len() as i32,
                &ts,
                std::ptr::null(),
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() == ErrorKind::Interrupted {
            Ok(0)
        } else {
            Err(err)
        }
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        // SAFETY: the descriptor is ours and closed once.
        unsafe { libc::close(self.0) };
    }
}

fn connect(port: u16) -> Result<TcpStream, String> {
    let s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    s.set_nodelay(true).map_err(|e| format!("nodelay: {e}"))?;
    s.set_nonblocking(true)
        .map_err(|e| format!("nonblocking: {e}"))?;
    Ok(s)
}

fn send(c: &mut Conn, req: &[u8], i: usize) -> Result<(), String> {
    while c.sent < req.len() {
        match c.sock.write(&req[c.sent..]) {
            Ok(n) => c.sent += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("send on connection {i}: {e}")),
        }
    }
    Ok(())
}

/// One thread's loop over its connections, from the schedule's start `t0`.
fn drive(o: &Opts, socks: Vec<TcpStream>, t0: Instant, seed: u64) -> Result<Tally, String> {
    let closed = matches!(o.load, Load::Closed);
    let (start, end) = (t0 + o.warm, t0 + o.warm + o.dur);
    let deadline = end + DRAIN;
    let gap = |t: Instant| Duration::from_secs_f64(o.conns as f64 / o.load.rate_at(t0, t));
    let ep = Epoll::new().map_err(|e| format!("epoll: {e}"))?;
    let mut rng = seed | 1;
    let mut heap = BinaryHeap::with_capacity(socks.len());
    let mut conns = Vec::with_capacity(socks.len());
    for (i, sock) in socks.into_iter().enumerate() {
        ep.add(sock.as_raw_fd(), i)
            .map_err(|e| format!("epoll_ctl: {e}"))?;
        let due = if closed {
            t0
        } else {
            t0 + gap(t0).mul_f64(unit(&mut rng))
        };
        heap.push(Reverse((due, i)));
        conns.push(Conn {
            sock,
            due,
            intended: due,
            sent: 0,
            got: 0,
            busy: false,
            late: false,
            done: false,
        });
    }
    let req = vec![0x3cu8; o.req];
    let mut scratch = vec![0u8; 64 * 1024];
    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 1024];
    let mut tally = Tally {
        hist: histogram(),
        lag: histogram(),
        completed: 0,
        unfinished: 0,
    };
    let (mut busy, mut responses) = (0usize, 0u64);
    loop {
        let now = Instant::now();
        while let Some(&Reverse((due, i))) = heap.peek() {
            if due > now {
                break;
            }
            heap.pop();
            let c = &mut conns[i];
            if if closed { now >= end } else { due >= end } {
                c.done = true;
                continue;
            }
            c.intended = if closed { now } else { due };
            if !closed && !c.late && due >= start {
                tally.lag.saturating_record(nanos(now - due));
            }
            (c.busy, c.sent, c.got) = (true, 0, 0);
            busy += 1;
            send(c, &req, i)?;
        }
        if busy == 0 && heap.is_empty() {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let next = heap.peek().map_or(deadline, |r| r.0.0.min(deadline));
        let wait = next.saturating_duration_since(now);
        let timeout = if o.spin || wait <= SPIN_WITHIN {
            Duration::ZERO
        } else {
            wait - SPIN_WITHIN / 2
        };
        let n = ep
            .wait(&mut events, timeout)
            .map_err(|e| format!("epoll_wait: {e}"))?;
        for ev in &events[..n] {
            let (flags, i) = (ev.events, ev.u64 as usize);
            let c = &mut conns[i];
            if flags & (libc::EPOLLIN | libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 {
                loop {
                    match c.sock.read(&mut scratch) {
                        Ok(0) => return Err(format!("the server closed connection {i}")),
                        Ok(n) => {
                            if !c.busy {
                                return Err(format!(
                                    "connection {i} received {n} bytes with no request in flight"
                                ));
                            }
                            if c.got + n > o.resp {
                                return Err(format!(
                                    "connection {i} received {} bytes past the {}-byte response",
                                    c.got + n - o.resp,
                                    o.resp
                                ));
                            }
                            if responses % CHECK_EVERY == 0
                                && scratch[..n].iter().any(|&b| b != RESPONSE_BYTE)
                            {
                                return Err(format!(
                                    "connection {i} received a byte the server never sends"
                                ));
                            }
                            c.got += n;
                            if c.got < o.resp {
                                continue;
                            }
                            let t = Instant::now();
                            responses += 1;
                            c.busy = false;
                            busy -= 1;
                            if c.intended >= start && c.intended < end {
                                tally.hist.saturating_record(nanos(t - c.intended));
                                tally.completed += 1;
                            }
                            c.due = if closed {
                                t
                            } else {
                                c.intended + gap(c.intended)
                            };
                            c.late = c.due <= t;
                            heap.push(Reverse((c.due, i)));
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(e) => return Err(format!("recv on connection {i}: {e}")),
                    }
                }
            }
            if flags & libc::EPOLLOUT as u32 != 0 && c.busy && c.sent < o.req {
                send(c, &req, i)?;
            }
        }
    }
    if !closed {
        let stop = Instant::now();
        for c in conns.iter().filter(|c| !c.done) {
            let mut due = c.due;
            if c.busy {
                if c.intended >= start {
                    tally.unfinished += 1;
                    tally.hist.saturating_record(nanos(stop - c.intended));
                }
                due = c.intended + gap(c.intended);
            }
            while due < end {
                if due >= start {
                    tally.unfinished += 1;
                    tally.hist.saturating_record(nanos(stop - due));
                }
                due += gap(due);
            }
        }
    }
    Ok(tally)
}

fn loadavg() -> String {
    let s = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let f: Vec<&str> = s.split_whitespace().collect();
    format!(
        "load1={} load5={}",
        f.first().copied().unwrap_or("na"),
        f.get(1).copied().unwrap_or("na")
    )
}

fn sleep_until(t: Instant) {
    std::thread::sleep(t.saturating_duration_since(Instant::now()));
}

fn fail(msg: &str) -> ! {
    println!("error=\"{msg}\"");
    std::process::exit(1);
}

fn main() {
    let nofile = raise_nofile();
    let o = parse().unwrap_or_else(|e| {
        eprintln!("{e}");
        eprintln!(
            "usage: client port=P conns=C req=B resp=B load=closed|steady|burst [rate=R] \
             [low=L high=H period_ms=P] warm=S dur=S pid=PID cpus=a,b,.. [spin=0|1]"
        );
        std::process::exit(2);
    });
    let threads = o.cpus.len().min(o.conns).max(1);
    let (ready_tx, ready_rx) = mpsc::channel::<Option<String>>();
    let mut go = Vec::with_capacity(threads);
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let share = o.conns / threads + usize::from(t < o.conns % threads);
            let (go_tx, go_rx) = mpsc::channel::<Instant>();
            go.push(go_tx);
            let (ready, o) = (ready_tx.clone(), o.clone());
            std::thread::spawn(move || -> Result<Tally, String> {
                pin(&[o.cpus[t]]);
                let socks: Result<Vec<_>, String> = (0..share).map(|_| connect(o.port)).collect();
                let _ = ready.send(socks.as_ref().err().cloned());
                let socks = socks?;
                let t0 = go_rx
                    .recv()
                    .map_err(|_| "the run never started".to_string())?;
                drive(
                    &o,
                    socks,
                    t0,
                    0x9e37_79b9_7f4a_7c15u64.wrapping_mul(t as u64 + 1),
                )
            })
        })
        .collect();
    for _ in 0..threads {
        if let Ok(Some(err)) = ready_rx.recv() {
            fail(&err);
        }
    }
    let t0 = Instant::now() + Duration::from_millis(20);
    for g in &go {
        let _ = g.send(t0);
    }
    let (start, end) = (t0 + o.warm, t0 + o.warm + o.dur);
    sleep_until(start);
    let c0 = cpu_ticks(o.pid);
    sleep_until(end);
    let c1 = cpu_ticks(o.pid);
    let load = loadavg();
    let mut all = Tally {
        hist: histogram(),
        lag: histogram(),
        completed: 0,
        unfinished: 0,
    };
    for h in handles {
        match h.join() {
            Ok(Ok(t)) => {
                all.hist.add(t.hist).expect("same bounds");
                all.lag.add(t.lag).expect("same bounds");
                all.completed += t.completed;
                all.unfinished += t.unfinished;
            }
            Ok(Err(e)) => fail(&e),
            Err(_) => fail("a client thread panicked"),
        }
    }
    let secs = o.dur.as_secs_f64();
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let cpu_s = c1.saturating_sub(c0) as f64 / hz;
    let us = |h: &Histogram<u64>, q: f64| {
        if h.is_empty() {
            "na".to_string()
        } else {
            format!("{:.1}", h.value_at_quantile(q) as f64 / 1e3)
        }
    };
    let max = |h: &Histogram<u64>| {
        if h.is_empty() {
            "na".to_string()
        } else {
            format!("{:.1}", h.max() as f64 / 1e3)
        }
    };
    let per_req = if all.completed == 0 || o.pid == 0 {
        "na".to_string()
    } else {
        format!("{:.2}", cpu_s * 1e6 / all.completed as f64)
    };
    println!(
        "rate={:.0} offered={:.0} p50_us={} p99_us={} p999_us={} max_us={} unfinished={} \
         cpu_us_per_req={} server_cpu_pct={:.1} lag_p99_us={} lag_max_us={} {} nofile={} nofile_before={}",
        all.completed as f64 / secs,
        (all.completed + all.unfinished) as f64 / secs,
        us(&all.hist, 0.5),
        us(&all.hist, 0.99),
        us(&all.hist, 0.999),
        max(&all.hist),
        all.unfinished,
        per_req,
        cpu_s / secs * 100.0,
        us(&all.lag, 0.99),
        max(&all.lag),
        load,
        nofile.1,
        nofile.0,
    );
}
