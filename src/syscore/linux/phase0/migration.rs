//! 0e: handle migration on epoll. One socket used from two shards: re-registered with the
//! using shard (strategy 1) or kept on its home shard and read through a mailbox (strategy 2).
//!
//! Model: a task reads `total` 1 KiB messages from one loopback socket accepted on shard A; the
//! schedule says which shard reads message `i`. Accept registration and end-of-run teardown
//! `epoll_ctl` calls are common to every strategy and are not counted; the writer is a third
//! pinned thread. `Backlog` keeps data ready (an op is pure overhead); `Paced` waits for
//! readiness on every read (an op is send-to-delivered latency).

use super::*;

const MSG: usize = 1024;
const HOME: usize = 0;
const REG_MASK: u32 = (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLRDHUP | libc::EPOLLET) as u32;
const READY: u32 = (libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLHUP | libc::EPOLLERR) as u32;
const BACKLOG_MSGS: u64 = 100_000;
const PACED_MSGS: u64 = 5_000;
const PACE_NS: u64 = 200_000;
const FIRST_USE_REPS: u64 = 2_000;

type Buf = Box<[u8; MSG]>;

#[derive(Clone, Copy, PartialEq)]
enum Strategy {
    /// Strategy 1: DEL on the old shard, ADD on the new one, generation-tagged tokens.
    Reregister,
    /// Strategy 2: the fd stays on the home shard; other shards proxy reads through it.
    HomeShard,
}

#[derive(Clone, Copy)]
enum Schedule {
    /// Message `i` is read by shard `(i / K) % 2`, starting on shard A.
    Alternate(u64),
    /// Never switch, shard A (the fd's birthplace).
    OnlyA,
    /// Never switch, shard B although the fd was accepted on A.
    OnlyB,
}

impl Schedule {
    fn shard(self, idx: u64) -> usize {
        match self {
            Schedule::Alternate(k) => (idx / k % 2) as usize,
            Schedule::OnlyA => 0,
            Schedule::OnlyB => 1,
        }
    }
    fn label(self) -> String {
        match self {
            Schedule::Alternate(k) => format!("K={k}"),
            Schedule::OnlyA => "never (on A)".into(),
            Schedule::OnlyB => "never (A->B once)".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Regime {
    /// The writer is far ahead: data is always ready, so an op measures pure overhead.
    Backlog,
    /// One message per `PACE_NS`: every read waits for readiness; an op is send-to-delivered.
    Paced,
}

struct Conn {
    sock: TcpStream,
    gen_: AtomicU64,
    done: AtomicBool,
    t_start: AtomicU64,
    t_end: AtomicU64,
}

struct Task {
    conn: Arc<Conn>,
    idx: u64,
    op_t0: u64,
    hop: u64,
    buf: Option<Buf>,
    filled: usize,
    waiting: bool,
}

struct Pending {
    from: usize,
    conn: Arc<Conn>,
    buf: Buf,
    filled: usize,
    waiting: bool,
}

enum Msg {
    /// The connection was accepted on this shard.
    Accept(Arc<Conn>),
    /// The task now runs here; `gen_` is the registration generation (strategy 1 only).
    Run {
        task: Task,
        gen_: u64,
    },
    /// Strategy 2: proxied read request for the home shard.
    Read {
        from: usize,
        conn: Arc<Conn>,
        buf: Buf,
        filled: usize,
    },
    /// Strategy 2: the home shard's answer, a whole message in `buf`.
    Done(Buf),
    Stop,
}

struct World {
    strategy: Strategy,
    schedule: Schedule,
    regime: Regime,
    total: u64,
    mail: [Wake<Msg>; 2],
    clock: Clock,
    gens: AtomicU64,
}

impl World {
    fn new(strategy: Strategy, schedule: Schedule, regime: Regime, total: u64) -> Self {
        Self {
            strategy,
            schedule,
            regime,
            total,
            mail: [Wake::new(), Wake::new()],
            clock: Clock::new(),
            gens: AtomicU64::new(1),
        }
    }
}

#[derive(Default)]
struct ShardStats {
    ctl: u64,
    wait: u64,
    rd: u64,
    efd_wr: u64,
    efd_rd: u64,
    stale: u64,
    spurious: u64,
    out_only: u64,
    migrations: u64,
    bad: u64,
    bad_owner: u64,
    lost: u64,
    csw: i64,
}

struct SOut {
    st: ShardStats,
    ops: Vec<u32>,
    first: Vec<[u32; 2]>,
}

fn fill_msg(buf: &mut [u8], seq: u64, ts: u64) {
    buf[..8].copy_from_slice(&seq.to_le_bytes());
    buf[8..16].copy_from_slice(&ts.to_le_bytes());
    buf[16..].fill(seq as u8);
}

fn check_msg(buf: &[u8; MSG], idx: u64) -> bool {
    buf[..8] == idx.to_le_bytes() && buf[16] == idx as u8 && buf[MSG - 1] == idx as u8
}

fn tcp_pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let client = TcpStream::connect(l.local_addr().expect("addr")).expect("connect");
    let (server, _) = l.accept().expect("accept");
    server.set_nonblocking(true).expect("nonblocking");
    client.set_nodelay(true).expect("nodelay");
    (client, server)
}

struct Shard<'a> {
    w: &'a World,
    me: usize,
    task: Option<Task>,
    pending: Option<Pending>,
    reg: Option<Arc<Conn>>,
    reg_gen: u64,
    out: SOut,
    stop: bool,
}

impl<'a> Shard<'a> {
    fn new(w: &'a World, me: usize) -> Self {
        let cap = w.total as usize;
        Self {
            w,
            me,
            task: None,
            pending: None,
            reg: None,
            reg_gen: 0,
            out: SOut {
                st: ShardStats::default(),
                ops: Vec::with_capacity(cap),
                first: Vec::new(),
            },
            stop: false,
        }
    }

    fn ep(&self) -> RawFd {
        self.w.mail[self.me].ep.as_raw_fd()
    }

    fn send(&mut self, to: usize, m: Msg) {
        self.out.st.efd_wr += u64::from(self.w.mail[to].send::<SeqCstBoth>(m));
    }

    /// `counted` is false for accept and teardown calls, which are common to every strategy.
    fn register(&mut self, conn: &Arc<Conn>, gen_: u64, counted: bool) {
        let fd = conn.sock.as_raw_fd();
        epoll_ctl(
            self.ep(),
            libc::EPOLL_CTL_ADD,
            fd,
            REG_MASK,
            (gen_ << 1) | 1,
        )
        .expect("EPOLL_CTL_ADD");
        self.out.st.ctl += u64::from(counted);
        self.reg = Some(Arc::clone(conn));
        self.reg_gen = gen_;
    }

    fn deregister(&mut self, counted: bool) {
        if let Some(c) = self.reg.take() {
            epoll_ctl(self.ep(), libc::EPOLL_CTL_DEL, c.sock.as_raw_fd(), 0, 0)
                .expect("EPOLL_CTL_DEL");
            self.out.st.ctl += u64::from(counted);
        }
    }

    /// Nonblocking read of the rest of one message; true when `buf` holds all of it.
    fn read_some(&mut self, conn: &Conn, buf: &mut [u8; MSG], filled: &mut usize) -> bool {
        loop {
            self.out.st.rd += 1;
            // SAFETY: the pointer and length stay inside `buf` (`filled < MSG`).
            let n = unsafe {
                libc::read(
                    conn.sock.as_raw_fd(),
                    buf[*filled..].as_mut_ptr().cast(),
                    MSG - *filled,
                )
            };
            match n {
                0 => panic!("unexpected EOF"),
                1.. => {
                    *filled += n as usize;
                    if *filled == MSG {
                        return true;
                    }
                }
                _ => match io::Error::last_os_error().kind() {
                    io::ErrorKind::WouldBlock => return false,
                    io::ErrorKind::Interrupted => {}
                    _ => die("read"),
                },
            }
        }
    }

    /// The shard that reads directly must hold the connection's current registration.
    fn check_owner(&mut self, conn: &Arc<Conn>) {
        let ok = self
            .reg
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, conn) && conn.gen_.load(Acquire) == self.reg_gen);
        self.out.st.bad_owner += u64::from(!ok);
    }

    fn complete(&mut self, t: &mut Task) {
        let now = self.w.clock.ns();
        let buf = t.buf.as_ref().expect("buffer");
        self.out.st.bad += u64::from(!check_msg(buf, t.idx));
        let sent = u64::from_le_bytes(buf[8..16].try_into().expect("8 bytes"));
        let cost = match self.w.regime {
            Regime::Backlog => now - t.op_t0,
            Regime::Paced => now.saturating_sub(sent),
        };
        self.out.ops.push(cost.min(u64::from(u32::MAX)) as u32);
        if t.idx == 0 {
            let total = (now - t.op_t0).min(u64::from(u32::MAX)) as u32;
            self.out
                .first
                .push([total, t.hop.min(u64::from(u32::MAX)) as u32]);
        }
        t.idx += 1;
        t.op_t0 = now;
        t.filled = 0;
    }

    fn handle(&mut self, m: Msg) {
        let now = self.w.clock.ns();
        match m {
            Msg::Accept(conn) => {
                self.deregister(false);
                let g = conn.gen_.load(Relaxed);
                self.register(&conn, g, false);
                conn.t_start.store(now, Relaxed);
                self.task = Some(Task {
                    conn,
                    idx: 0,
                    op_t0: now,
                    hop: 0,
                    buf: Some(Box::new([0; MSG])),
                    filled: 0,
                    waiting: false,
                });
            }
            Msg::Run { mut task, gen_ } => {
                if task.idx == 0 {
                    task.hop = now - task.op_t0;
                }
                if self.w.strategy == Strategy::Reregister {
                    let conn = Arc::clone(&task.conn);
                    self.register(&conn, gen_, true);
                }
                self.task = Some(task);
            }
            Msg::Read {
                from,
                conn,
                buf,
                filled,
            } => {
                self.pending = Some(Pending {
                    from,
                    conn,
                    buf,
                    filled,
                    waiting: false,
                });
            }
            Msg::Done(buf) => {
                let mut t = self.task.take().expect("task awaiting a proxied read");
                t.buf = Some(buf);
                t.filled = MSG;
                t.waiting = false;
                self.complete(&mut t);
                self.task = Some(t);
            }
            Msg::Stop => self.stop = true,
        }
    }

    fn migrate(&mut self, t: Task, want: usize) {
        self.out.st.migrations += 1;
        let mut gen_ = 0;
        if self.w.strategy == Strategy::Reregister {
            self.deregister(true);
            gen_ = t.conn.gen_.fetch_add(1, AcqRel) + 1;
        }
        self.send(want, Msg::Run { task: t, gen_ });
    }

    fn finish(&mut self, t: Task) {
        t.conn.t_end.store(self.w.clock.ns(), Relaxed);
        self.deregister(false);
        t.conn.done.store(true, Release);
    }

    /// Runs the task on this shard until it needs to wait, migrates, or finishes.
    fn drive(&mut self) {
        while let Some(mut t) = self.task.take() {
            if t.waiting {
                self.task = Some(t);
                return;
            }
            if t.idx >= self.w.total {
                return self.finish(t);
            }
            let want = self.w.schedule.shard(t.idx);
            if want != self.me {
                return self.migrate(t, want);
            }
            if self.w.strategy == Strategy::HomeShard && self.me != HOME {
                let (conn, filled) = (Arc::clone(&t.conn), t.filled);
                let buf = t.buf.take().expect("buffer");
                t.waiting = true;
                self.task = Some(t);
                return self.send(
                    HOME,
                    Msg::Read {
                        from: self.me,
                        conn,
                        buf,
                        filled,
                    },
                );
            }
            self.check_owner(&t.conn);
            let (conn, buf) = (Arc::clone(&t.conn), t.buf.as_mut().expect("buffer"));
            if self.read_some(&conn, buf, &mut t.filled) {
                self.complete(&mut t);
            } else {
                t.waiting = true;
            }
            self.task = Some(t);
        }
    }

    /// Strategy 2 home shard: serve the proxied read once the socket may have data.
    fn serve_pending(&mut self) {
        let Some(mut p) = self.pending.take() else {
            return;
        };
        if p.waiting {
            self.pending = Some(p);
            return;
        }
        self.check_owner(&p.conn);
        let conn = Arc::clone(&p.conn);
        if self.read_some(&conn, &mut p.buf, &mut p.filled) {
            self.send(p.from, Msg::Done(p.buf));
        } else {
            p.waiting = true;
            self.pending = Some(p);
        }
    }

    fn on_socket(&mut self, token: u64, mask: u32) {
        let cur = self.reg.as_ref().map(|c| c.gen_.load(Acquire));
        if cur != Some(token >> 1) {
            self.out.st.stale += 1;
        } else if mask & READY == 0 {
            self.out.st.out_only += 1;
        } else if let Some(t) = self.task.as_mut().filter(|t| t.waiting) {
            t.waiting = false;
        } else if let Some(p) = self.pending.as_mut().filter(|p| p.waiting) {
            p.waiting = false;
        } else {
            self.out.st.spurious += 1;
        }
    }

    fn run(mut self) -> SOut {
        let w = self.w;
        let wk = &w.mail[self.me];
        let mut ev = [EMPTY_EV; 8];
        loop {
            while let Some(m) = wk.inbox.pop() {
                self.handle(m);
            }
            self.serve_pending();
            self.drive();
            if self.stop {
                break;
            }
            if !wk.arm::<SeqCstBoth>() {
                continue;
            }
            self.out.st.wait += 1;
            let n = wk.wait(&mut ev, 100);
            if n == 0 {
                self.out.st.lost += u64::from(!wk.inbox.is_empty());
            }
            for e in &ev[..n] {
                if tok(e) == EFD_TOKEN {
                    self.out.st.efd_rd += 1;
                } else {
                    self.on_socket(tok(e), evmask(e));
                }
            }
        }
        self.out.st.csw = involuntary_csw();
        self.out
    }
}

fn new_conn(world: &World, server: TcpStream) -> Arc<Conn> {
    Arc::new(Conn {
        sock: server,
        gen_: AtomicU64::new(world.gens.fetch_add(1 << 20, Relaxed)),
        done: AtomicBool::new(false),
        t_start: AtomicU64::new(0),
        t_end: AtomicU64::new(0),
    })
}

/// Sends `Stop` to both shards on drop, so a panicking harness does not hang the thread scope.
struct StopShards<'a>(&'a World);

impl Drop for StopShards<'_> {
    fn drop(&mut self) {
        for m in &self.0.mail {
            m.send::<SeqCstBoth>(Msg::Stop);
        }
    }
}

/// Starts both shards (pinned to `cpus[0]`, `cpus[1]`), runs `body`, stops them.
fn with_world(world: &World, cpus: Option<&[usize]>, body: impl FnOnce()) -> [SOut; 2] {
    thread::scope(|sc| {
        let hs: [_; 2] = std::array::from_fn(|me| {
            sc.spawn(move || {
                if let Some(c) = cpus {
                    pin(c[me]);
                }
                Shard::new(world, me).run()
            })
        });
        {
            let _stop = StopShards(world);
            body();
        }
        hs.map(|h| h.join().expect("shard"))
    })
}

fn writer(mut s: TcpStream, world: &World, cpu: Option<usize>) {
    if let Some(c) = cpu {
        pin(c);
    }
    const CHUNK: u64 = 64;
    let mut buf = vec![0u8; CHUNK as usize * MSG];
    let start = world.clock.ns() + 1_000_000;
    let mut seq = 0;
    while seq < world.total {
        let n = match world.regime {
            Regime::Backlog => CHUNK.min(world.total - seq),
            Regime::Paced => 1,
        };
        if world.regime == Regime::Paced {
            while world.clock.ns() < start + seq * PACE_NS {
                std::hint::spin_loop();
            }
        }
        let ts = world.clock.ns();
        for i in 0..n as usize {
            fill_msg(&mut buf[i * MSG..(i + 1) * MSG], seq + i as u64, ts);
        }
        s.write_all(&buf[..n as usize * MSG]).expect("write");
        seq += n;
    }
}

struct Steady {
    outs: [SOut; 2],
    elapsed_ns: u64,
}

fn run_steady(
    strategy: Strategy,
    schedule: Schedule,
    regime: Regime,
    total: u64,
    cpus: Option<&[usize]>,
) -> Steady {
    let world = World::new(strategy, schedule, regime, total);
    let mut elapsed_ns = 0;
    let outs = with_world(&world, cpus, || {
        let (client, server) = tcp_pair();
        let conn = new_conn(&world, server);
        thread::scope(|sc| {
            let wr = sc.spawn(|| writer(client, &world, cpus.map(|c| c[2])));
            world.mail[0].send::<SeqCstBoth>(Msg::Accept(Arc::clone(&conn)));
            spin_until("connection done", || conn.done.load(Acquire));
            wr.join().expect("writer");
        });
        elapsed_ns = conn.t_end.load(Relaxed) - conn.t_start.load(Relaxed);
    });
    Steady { outs, elapsed_ns }
}

// Column indices of a steady-state row.
const S_MIGR: usize = 10;
const S_STALE: usize = 11;
const S_LOST: usize = 13;
const S_BAD: usize = 14;

fn summarize_steady(total: u64, s: &Steady) -> Vec<f64> {
    let ops = sorted(s.outs.iter().flat_map(|o| o.ops.iter().copied()).collect());
    let sum = |f: fn(&ShardStats) -> u64| s.outs.iter().map(|o| f(&o.st)).sum::<u64>() as f64;
    let per = |x: f64| x / total as f64;
    vec![
        total as f64 / s.elapsed_ns as f64 * 1e6,
        quant(&ops, 0.5),
        quant(&ops, 0.99),
        quant(&ops, 0.999),
        f64::from(ops.last().copied().unwrap_or(0)),
        per(sum(|s| s.ctl)),
        per(sum(|s| s.wait)),
        per(sum(|s| s.rd)),
        per(sum(|s| s.efd_wr)),
        per(sum(|s| s.efd_rd)),
        sum(|s| s.migrations),
        sum(|s| s.stale),
        sum(|s| s.spurious + s.out_only),
        sum(|s| s.lost),
        sum(|s| s.bad + s.bad_owner),
        s.outs.iter().map(|o| o.st.csw).sum::<i64>() as f64,
    ]
}

/// Many fresh connections, each used for exactly one message that is already waiting.
fn run_first_use(strategy: Strategy, schedule: Schedule, cpus: Option<&[usize]>) -> Vec<f64> {
    let world = World::new(strategy, schedule, Regime::Backlog, 1);
    let outs = with_world(&world, cpus, || {
        let mut msg = [0u8; MSG];
        fill_msg(&mut msg, 0, 0);
        for _ in 0..FIRST_USE_REPS {
            let (mut client, server) = tcp_pair();
            client.write_all(&msg).expect("write");
            let conn = new_conn(&world, server);
            world.mail[0].send::<SeqCstBoth>(Msg::Accept(Arc::clone(&conn)));
            spin_until("first use", || conn.done.load(Acquire));
            // Let both shards park again so every rep starts from the same state.
            spin_for(world.clock, 100_000);
        }
    });
    let first: Vec<[u32; 2]> = outs.iter().flat_map(|o| o.first.iter().copied()).collect();
    let total = sorted(first.iter().map(|f| f[0]).collect());
    let hop = sorted(first.iter().map(|f| f[1]).collect());
    let spec = sorted(first.iter().map(|f| f[0].saturating_sub(f[1])).collect());
    let sum = |f: fn(&ShardStats) -> u64| outs.iter().map(|o| f(&o.st)).sum::<u64>() as f64;
    let per = |x: f64| x / FIRST_USE_REPS as f64;
    vec![
        first.len() as f64,
        quant(&total, 0.5),
        quant(&total, 0.99),
        quant(&hop, 0.5),
        quant(&spec, 0.5),
        per(sum(|s| s.ctl)),
        per(sum(|s| s.wait)),
        per(sum(|s| s.rd)),
        per(sum(|s| s.efd_wr)),
        per(sum(|s| s.efd_rd)),
        sum(|s| s.bad + s.bad_owner),
    ]
}

const STEADY_COLS: &[(&str, usize)] = &[
    ("kops/s", 0),
    ("op_med", 1),
    ("op_p99", 2),
    ("op_p999", 3),
    ("op_max", 4),
    ("ctl/op", 5),
    ("wait/op", 6),
    ("read/op", 7),
    ("efdw/op", 8),
    ("efdr/op", 9),
    ("migr", 10),
    ("stale", 11),
    ("spurious", 12),
    ("lost", 13),
    ("bad", 14),
    ("inv_csw", 15),
];

#[test]
#[ignore]
fn phase0_migration_steady() {
    let _exclusive = exclusive();
    let cpus = pick_cpus(3);
    banner(
        "0e handle migration, steady state (A=cpu0 of list, B=cpu1, writer=cpu2)",
        &cpus,
    );
    for (regime, total, what) in [
        (
            Regime::Backlog,
            BACKLOG_MSGS,
            "backlog: op = completion-to-completion, ns",
        ),
        (
            Regime::Paced,
            PACED_MSGS,
            "paced 1 msg per 200us: op = send-to-delivered, ns",
        ),
    ] {
        let (mut rows, mut spread) = (vec![], vec![]);
        for (strategy, name) in [
            (Strategy::Reregister, "S1 re-register"),
            (Strategy::HomeShard, "S2 home shard"),
        ] {
            for sched in [
                Schedule::Alternate(1),
                Schedule::Alternate(16),
                Schedule::Alternate(1024),
                Schedule::OnlyA,
                Schedule::OnlyB,
            ] {
                let runs: Vec<_> = (0..RUNS)
                    .map(|_| {
                        summarize_steady(
                            total,
                            &run_steady(strategy, sched, regime, total, Some(&cpus)),
                        )
                    })
                    .collect();
                let label = format!("{name} {}", sched.label());
                spread.extend(spread_rows(&label, &runs));
                rows.push((label, median_cols(&runs)));
            }
        }
        table(
            &format!("0e steady state, {what}, {total} msgs, median of {RUNS} runs"),
            STEADY_COLS,
            &rows,
        );
        table(
            &format!("0e steady state spread over {RUNS} runs ({what})"),
            &[
                ("kops/s", 0),
                ("op_med", 1),
                ("op_p99", 2),
                ("op_p999", 3),
                ("inv_csw", 15),
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
}

#[test]
#[ignore]
fn phase0_migration_first_use() {
    let _exclusive = exclusive();
    let cpus = pick_cpus(3);
    banner(
        "0e first use on B after accept on A (A=cpu0 of list, B=cpu1)",
        &cpus,
    );
    let (mut rows, mut spread) = (vec![], vec![]);
    for (label, strategy, sched) in [
        (
            "home A, no migration",
            Strategy::Reregister,
            Schedule::OnlyA,
        ),
        ("S1 re-register A->B", Strategy::Reregister, Schedule::OnlyB),
        (
            "S2 home shard, used on B",
            Strategy::HomeShard,
            Schedule::OnlyB,
        ),
    ] {
        let runs: Vec<_> = (0..RUNS)
            .map(|_| run_first_use(strategy, sched, Some(&cpus)))
            .collect();
        spread.extend(spread_rows(label, &runs));
        rows.push((label.to_owned(), median_cols(&runs)));
    }
    table(
        &format!(
            "0e first use, {FIRST_USE_REPS} fresh connections per run, median of {RUNS} runs (ns; counts per first use)"
        ),
        &[
            ("n", 0),
            ("total_med", 1),
            ("total_p99", 2),
            ("hop_med", 3),
            ("strat_med", 4),
            ("ctl", 5),
            ("wait", 6),
            ("read", 7),
            ("efdw", 8),
            ("efdr", 9),
            ("bad", 10),
        ],
        &rows,
    );
    table(
        &format!("0e first use spread over {RUNS} runs (ns)"),
        &[("total_med", 1), ("hop_med", 3), ("strat_med", 4)],
        &spread,
    );
    println!(
        "loadavg after: {}",
        std::fs::read_to_string("/proc/loadavg")
            .unwrap_or_default()
            .trim()
    );
}

fn steady_check(strategy: Strategy, sched: Schedule, total: u64) -> Vec<f64> {
    let s = run_steady(strategy, sched, Regime::Backlog, total, None);
    summarize_steady(total, &s)
}

#[test]
fn reregister_survives_10k_switches() {
    let r = steady_check(Strategy::Reregister, Schedule::Alternate(1), 10_000);
    assert_eq!(
        r[S_MIGR], 9_999.0,
        "every op after the first must switch shards"
    );
    assert_eq!(
        r[S_BAD], 0.0,
        "data lost, duplicated, or read by a non-owner"
    );
    assert_eq!(r[S_LOST], 0.0, "lost wakeup");
}

#[test]
fn home_shard_proxy_delivers_in_order() {
    let r = steady_check(Strategy::HomeShard, Schedule::Alternate(1), 2_000);
    assert_eq!(
        r[S_BAD], 0.0,
        "data lost, duplicated, or read by a non-owner"
    );
    assert_eq!(r[S_STALE], 0.0, "the home shard never changes registration");
}

#[test]
fn stale_generation_event_is_ignored() {
    // An event harvested from epoll before the migration's DEL must not count as readiness
    // afterwards: DEL does not recall events already copied out.
    let (mut client, server) = tcp_pair();
    let (old, new) = (epoll_new(), epoll_new());
    let conn = Arc::new(Conn {
        sock: server,
        gen_: AtomicU64::new(1),
        done: AtomicBool::new(false),
        t_start: AtomicU64::new(0),
        t_end: AtomicU64::new(0),
    });
    let fd = conn.sock.as_raw_fd();
    epoll_ctl(
        old.as_raw_fd(),
        libc::EPOLL_CTL_ADD,
        fd,
        REG_MASK,
        (1 << 1) | 1,
    )
    .expect("add old");
    client.write_all(&[1u8; MSG]).expect("write");
    let mut harvested = [EMPTY_EV; 4];
    // SAFETY: valid buffer of 4 events.
    let n = unsafe { libc::epoll_wait(old.as_raw_fd(), harvested.as_mut_ptr(), 4, 1000) };
    assert!(n > 0, "event for the old registration");
    // Migrate: DEL on the old set, bump the generation, ADD on the new set.
    epoll_ctl(old.as_raw_fd(), libc::EPOLL_CTL_DEL, fd, 0, 0).expect("del");
    let cur = conn.gen_.fetch_add(1, AcqRel) + 1;
    epoll_ctl(
        new.as_raw_fd(),
        libc::EPOLL_CTL_ADD,
        fd,
        REG_MASK,
        (cur << 1) | 1,
    )
    .expect("add new");
    // The old set delivers nothing after DEL, even with fresh data.
    client.write_all(&[2u8; MSG]).expect("write");
    // SAFETY: valid buffer of 4 events.
    assert_eq!(
        unsafe { libc::epoll_wait(old.as_raw_fd(), [EMPTY_EV; 4].as_mut_ptr(), 4, 0) },
        0
    );
    let stale = harvested[..n as usize]
        .iter()
        .filter(|e| tok(e) >> 1 != cur)
        .count();
    assert_eq!(
        stale, n as usize,
        "the harvested event carries the old generation"
    );
    let mut fresh = [EMPTY_EV; 4];
    // SAFETY: valid buffer of 4 events.
    let m = unsafe { libc::epoll_wait(new.as_raw_fd(), fresh.as_mut_ptr(), 4, 1000) };
    assert!(m > 0 && fresh[..m as usize].iter().all(|e| tok(e) >> 1 == cur));
}

#[test]
fn waiting_proxy_request_survives_a_serve_pass() {
    // A request that hit EAGAIN stays queued at the home shard until the socket is readable;
    // serving it again before then must not drop it.
    let world = World::new(
        Strategy::HomeShard,
        Schedule::Alternate(1),
        Regime::Backlog,
        1,
    );
    let (_client, server) = tcp_pair();
    let conn = new_conn(&world, server);
    let mut shard = Shard::new(&world, HOME);
    shard.pending = Some(Pending {
        from: 1,
        conn,
        buf: Box::new([0; MSG]),
        filled: 0,
        waiting: true,
    });
    shard.serve_pending();
    assert!(shard.pending.as_ref().is_some_and(|p| p.waiting));
}
