use echo::pin;
use hdrhistogram::Histogram;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn cpu_ticks(pid: u32) -> u64 {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let rest = s.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    let at = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    at(11) + at(12)
}

async fn conn(
    port: u16,
    msg: usize,
    start: Instant,
    end: Instant,
    hist: Rc<RefCell<Histogram<u64>>>,
    ops: Rc<RefCell<u64>>,
) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.set_nodelay(true).unwrap();
    let out = vec![0xabu8; msg];
    let mut back = vec![0u8; msg];
    loop {
        let t0 = Instant::now();
        if t0 >= end {
            break;
        }
        s.write_all(&out).await.unwrap();
        s.read_exact(&mut back).await.unwrap();
        if t0 >= start {
            let ns = t0.elapsed().as_nanos() as u64;
            hist.borrow_mut().saturating_record(ns.max(1));
            *ops.borrow_mut() += 1;
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let num = |i: usize| a[i].parse::<u64>().unwrap();
    let (port, conns, msg, warm, dur, pid) = (
        num(1) as u16,
        num(2) as usize,
        num(3) as usize,
        num(4),
        num(5),
        num(6) as u32,
    );
    let cpus: Vec<usize> = a[7].split(',').map(|c| c.parse().unwrap()).collect();
    let threads = cpus.len().min(conns);
    let start = Instant::now() + Duration::from_secs(1 + warm);
    let end = start + Duration::from_secs(dur);
    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let share = conns / threads + usize::from(t < conns % threads);
            let cpu = cpus[t];
            std::thread::spawn(move || {
                pin(&[cpu]);
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let local = tokio::task::LocalSet::new();
                let hist = Rc::new(RefCell::new(
                    Histogram::<u64>::new_with_max(60_000_000_000, 3).unwrap(),
                ));
                let ops = Rc::new(RefCell::new(0u64));
                local.block_on(&rt, async {
                    let js: Vec<_> = (0..share)
                        .map(|_| {
                            tokio::task::spawn_local(conn(
                                port,
                                msg,
                                start,
                                end,
                                hist.clone(),
                                ops.clone(),
                            ))
                        })
                        .collect();
                    for j in js {
                        j.await.unwrap();
                    }
                });
                let h = hist.borrow().clone();
                let n = *ops.borrow();
                (h, n)
            })
        })
        .collect();
    std::thread::sleep(start.saturating_duration_since(Instant::now()));
    let t0 = cpu_ticks(pid);
    std::thread::sleep(end.saturating_duration_since(Instant::now()));
    let t1 = cpu_ticks(pid);
    let mut hist = Histogram::<u64>::new_with_max(60_000_000_000, 3).unwrap();
    let mut total = 0u64;
    for h in handles {
        let (hh, n) = h.join().unwrap();
        hist.add(hh).unwrap();
        total += n;
    }
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let cpu = (t1 - t0) as f64 / hz / dur as f64 * 100.0;
    let us = |q: f64| hist.value_at_quantile(q) as f64 / 1000.0;
    println!(
        "{{\"qps\":{:.0},\"p50\":{:.1},\"p99\":{:.1},\"p999\":{:.1},\"cpu\":{:.1}}}",
        total as f64 / dur as f64,
        us(0.5),
        us(0.99),
        us(0.999),
        cpu
    );
}
