//! Every row of `features` and `run_loop`, run on nuclei and on tokio with the
//! same workload: one criterion group per row, with the functions `nuclei` and
//! `tokio`. Ratio = nuclei time / tokio time; below 1 means nuclei is faster.
//!
//! Run with `taskset -c 4-7 cargo bench --bench vs_tokio`.
//!
//! Runtime shapes, matched and unpinned on both sides:
//! - one thread: nuclei `worker_threads(1).pin_workers(false).handoff_after(None)`
//!   against tokio `new_current_thread().enable_all()`. Every row except the
//!   two below.
//! - two workers: nuclei `worker_threads(2)` against tokio
//!   `new_multi_thread().worker_threads(2).enable_all()`. Rows `mpsc_across_workers`
//!   and `oneshot_pairs`.
//!
//! The file row is the real-world difference, not a like-for-like engine
//! comparison: `tokio::fs` runs each read on its blocking thread pool, nuclei
//! reads through its own file IO. Socket options are the defaults on both sides.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn nuclei_rt(workers: usize) -> nuclei::runtime::Runtime {
    nuclei::runtime::Builder::new()
        .worker_threads(workers)
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .expect("the runtime starts")
}

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the runtime starts")
}

fn tokio_rt_multi(workers: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("the runtime starts")
}

fn group<'a>(
    c: &'a mut Criterion,
    name: &str,
    elements: Throughput,
    slow: bool,
) -> BenchmarkGroup<'a, WallTime> {
    let mut group = c.benchmark_group(name);
    group.throughput(elements);
    if slow {
        group.sample_size(20);
        group.warm_up_time(Duration::from_millis(500));
        group.measurement_time(Duration::from_secs(3));
    }
    group
}

fn block_on_ready(c: &mut Criterion) {
    let mut g = group(c, "block_on_ready", Throughput::Elements(1), false);
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| b.iter(|| rt.block_on(async { 1u8 })));
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| b.iter(|| rt.block_on(async { 1u8 })));
    g.finish();
}

fn spawn_join(c: &mut Criterion) {
    const SPAWNED: u64 = 1_000;
    let mut g = group(c, "spawn_join", Throughput::Elements(SPAWNED), false);
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..SPAWNED).map(|i| rt.spawn(async move { i })).collect();
            for task in tasks {
                rt.block_on(task);
            }
        })
    });
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..SPAWNED).map(|i| rt.spawn(async move { i })).collect();
            for task in tasks {
                rt.block_on(task).unwrap();
            }
        })
    });
    g.finish();
}

fn yield_loop(c: &mut Criterion) {
    const TASKS: u64 = 64;
    const YIELDS: u64 = 4_000;
    let mut g = group(c, "yield", Throughput::Elements(TASKS * YIELDS), false);
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..TASKS)
                .map(|_| {
                    rt.spawn(async {
                        for _ in 0..YIELDS {
                            nuclei::task::yield_now().await;
                        }
                    })
                })
                .collect();
            for task in tasks {
                rt.block_on(task);
            }
        })
    });
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..TASKS)
                .map(|_| {
                    rt.spawn(async {
                        for _ in 0..YIELDS {
                            tokio::task::yield_now().await;
                        }
                    })
                })
                .collect();
            for task in tasks {
                rt.block_on(task).unwrap();
            }
        })
    });
    g.finish();
}

fn mpsc_ping_pong(c: &mut Criterion) {
    const ROUNDS: u64 = 2_000;
    let mut g = group(c, "mpsc_ping_pong", Throughput::Elements(ROUNDS * 2), false);
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let (to_b, mut from_a) = nuclei::sync::mpsc::channel::<u64>(1);
            let (to_a, mut from_b) = nuclei::sync::mpsc::channel::<u64>(1);
            let a = rt.spawn(async move {
                for i in 0..ROUNDS {
                    to_b.send(i).await.unwrap();
                    from_b.recv().await.unwrap();
                }
            });
            let echo = rt.spawn(async move {
                while let Some(i) = from_a.recv().await {
                    if to_a.send(i).await.is_err() {
                        break;
                    }
                }
            });
            rt.block_on(a);
            rt.block_on(echo);
        })
    });
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let (to_b, mut from_a) = tokio::sync::mpsc::channel::<u64>(1);
            let (to_a, mut from_b) = tokio::sync::mpsc::channel::<u64>(1);
            let a = rt.spawn(async move {
                for i in 0..ROUNDS {
                    to_b.send(i).await.unwrap();
                    from_b.recv().await.unwrap();
                }
            });
            let echo = rt.spawn(async move {
                while let Some(i) = from_a.recv().await {
                    if to_a.send(i).await.is_err() {
                        break;
                    }
                }
            });
            rt.block_on(a).unwrap();
            rt.block_on(echo).unwrap();
        })
    });
    g.finish();
}

fn oneshot_pairs(c: &mut Criterion) {
    const PAIRS: u64 = 2_000;
    let mut g = group(c, "oneshot_pairs", Throughput::Elements(PAIRS), true);
    let rt = nuclei_rt(2);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..PAIRS)
                .map(|i| {
                    let (tx, rx) = nuclei::sync::oneshot::channel::<u64>();
                    rt.spawn(async move {
                        nuclei::runtime::spawn(async move {
                            let _ = tx.send(i);
                        })
                        .detach();
                        rx.await.unwrap()
                    })
                })
                .collect();
            for task in tasks {
                rt.block_on(task);
            }
        })
    });
    drop(rt);
    let rt = tokio_rt_multi(2);
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..PAIRS)
                .map(|i| {
                    let (tx, rx) = tokio::sync::oneshot::channel::<u64>();
                    rt.spawn(async move {
                        tokio::spawn(async move {
                            let _ = tx.send(i);
                        });
                        rx.await.unwrap()
                    })
                })
                .collect();
            for task in tasks {
                rt.block_on(task).unwrap();
            }
        })
    });
    g.finish();
}

fn mpsc_across_workers(c: &mut Criterion) {
    const MESSAGES: u64 = 20_000;
    let mut g = group(
        c,
        "mpsc_across_workers",
        Throughput::Elements(MESSAGES),
        true,
    );
    let rt = nuclei_rt(2);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let (tx, mut rx) = nuclei::sync::mpsc::channel::<u64>(256);
            let sender = rt.spawn(async move {
                for i in 0..MESSAGES {
                    tx.send(i).await.unwrap();
                }
            });
            let receiver = rt.spawn(async move {
                let mut n = 0;
                while rx.recv().await.is_some() {
                    n += 1;
                }
                n
            });
            rt.block_on(sender);
            assert_eq!(rt.block_on(receiver), MESSAGES);
        })
    });
    drop(rt);
    let rt = tokio_rt_multi(2);
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(256);
            let sender = rt.spawn(async move {
                for i in 0..MESSAGES {
                    tx.send(i).await.unwrap();
                }
            });
            let receiver = rt.spawn(async move {
                let mut n = 0;
                while rx.recv().await.is_some() {
                    n += 1;
                }
                n
            });
            rt.block_on(sender).unwrap();
            assert_eq!(rt.block_on(receiver).unwrap(), MESSAGES);
        })
    });
    g.finish();
}

fn arm_and_fire(c: &mut Criterion) {
    const ARMED: u64 = 1_000;
    let mut g = group(
        c,
        "arm_and_fire_1000_sleeps",
        Throughput::Elements(ARMED),
        true,
    );
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..ARMED)
                .map(|i| {
                    rt.spawn(async move {
                        nuclei::time::sleep(Duration::from_millis(2 + i % 8)).await;
                    })
                })
                .collect();
            for task in tasks {
                rt.block_on(task);
            }
        })
    });
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..ARMED)
                .map(|i| {
                    rt.spawn(async move {
                        tokio::time::sleep(Duration::from_millis(2 + i % 8)).await;
                    })
                })
                .collect();
            for task in tasks {
                rt.block_on(task).unwrap();
            }
        })
    });
    g.finish();
}

fn timeout_hit(c: &mut Criterion) {
    const HITS: u64 = 1_000;
    let mut g = group(c, "timeout_hit", Throughput::Elements(HITS), true);
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            rt.block_on(rt.spawn(async {
                for _ in 0..HITS {
                    nuclei::time::timeout(Duration::from_secs(1), async {})
                        .await
                        .unwrap();
                }
            }));
        })
    });
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            rt.block_on(rt.spawn(async {
                for _ in 0..HITS {
                    tokio::time::timeout(Duration::from_secs(1), async {})
                        .await
                        .unwrap();
                }
            }))
            .unwrap();
        })
    });
    g.finish();
}

fn timeout_miss(c: &mut Criterion) {
    const MISSES: u64 = 20;
    let mut g = group(c, "timeout_miss_1ms", Throughput::Elements(MISSES), true);
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            rt.block_on(rt.spawn(async {
                for _ in 0..MISSES {
                    let late = std::future::pending::<()>();
                    assert!(
                        nuclei::time::timeout(Duration::from_millis(1), late)
                            .await
                            .is_err()
                    );
                }
            }));
        })
    });
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            rt.block_on(rt.spawn(async {
                for _ in 0..MISSES {
                    let late = std::future::pending::<()>();
                    assert!(
                        tokio::time::timeout(Duration::from_millis(1), late)
                            .await
                            .is_err()
                    );
                }
            }))
            .unwrap();
        })
    });
    g.finish();
}

fn nuclei_echo_server(rt: &nuclei::runtime::Runtime) -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let listener = nuclei::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        tx.send(listener.local_addr().unwrap()).unwrap();
        while let Ok((conn, _)) = listener.accept().await {
            nuclei::runtime::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = conn.recv(&mut buf).await {
                    let mut at = 0;
                    while at < n {
                        match conn.send(&buf[at..n]).await {
                            Ok(sent) => at += sent,
                            Err(_) => return,
                        }
                    }
                }
            })
            .detach();
        }
    })
    .detach();
    rx.recv().unwrap()
}

fn tokio_echo_server(rt: &tokio::runtime::Runtime) -> SocketAddr {
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    rt.spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = conn.read(&mut buf).await {
                    if conn.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

async fn nuclei_exchange(conn: &nuclei::net::TcpStream, size: usize, rounds: u64) {
    let request = vec![7u8; size];
    let mut reply = vec![0u8; size];
    for _ in 0..rounds {
        let mut at = 0;
        while at < size {
            at += conn.send(&request[at..]).await.unwrap();
        }
        at = 0;
        while at < size {
            at += conn.recv(&mut reply[at..]).await.unwrap();
        }
    }
}

async fn tokio_exchange(conn: &mut tokio::net::TcpStream, size: usize, rounds: u64) {
    let request = vec![7u8; size];
    let mut reply = vec![0u8; size];
    for _ in 0..rounds {
        conn.write_all(&request).await.unwrap();
        conn.read_exact(&mut reply).await.unwrap();
    }
}

fn tcp_ping_pong(c: &mut Criterion) {
    const ROUNDS: u64 = 2_000;
    let mut g = group(
        c,
        "tcp_ping_pong_one_connection",
        Throughput::Elements(ROUNDS),
        true,
    );
    let rt = nuclei_rt(1);
    let addr = nuclei_echo_server(&rt);
    let conn = std::sync::Arc::new(
        rt.block_on(rt.spawn(async move { nuclei::net::TcpStream::connect(addr).await.unwrap() })),
    );
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let conn = std::sync::Arc::clone(&conn);
            rt.block_on(rt.spawn(async move { nuclei_exchange(&conn, 64, ROUNDS).await }));
        })
    });
    drop(conn);
    drop(rt);
    let rt = tokio_rt();
    let addr = tokio_echo_server(&rt);
    let conn = RefCell::new(Some(
        rt.block_on(tokio::net::TcpStream::connect(addr)).unwrap(),
    ));
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let mut c = conn.borrow_mut().take().unwrap();
            let c = rt
                .block_on(rt.spawn(async move {
                    tokio_exchange(&mut c, 64, ROUNDS).await;
                    c
                }))
                .unwrap();
            *conn.borrow_mut() = Some(c);
        })
    });
    g.finish();
}

fn tcp_256(c: &mut Criterion) {
    const CONNS: u64 = 256;
    const PER_CONN: u64 = 20;
    let mut g = group(
        c,
        "tcp_request_reply_256_connections",
        Throughput::Elements(CONNS * PER_CONN),
        true,
    );
    let rt = nuclei_rt(1);
    let addr = nuclei_echo_server(&rt);
    let conns: Vec<_> = (0..CONNS)
        .map(|_| {
            std::sync::Arc::new(rt.block_on(
                rt.spawn(async move { nuclei::net::TcpStream::connect(addr).await.unwrap() }),
            ))
        })
        .collect();
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let tasks: Vec<_> = conns
                .iter()
                .map(|conn| {
                    let conn = std::sync::Arc::clone(conn);
                    rt.spawn(async move { nuclei_exchange(&conn, 128, PER_CONN).await })
                })
                .collect();
            for task in tasks {
                rt.block_on(task);
            }
        })
    });
    drop(conns);
    drop(rt);
    let rt = tokio_rt();
    let addr = tokio_echo_server(&rt);
    let conns: RefCell<Vec<_>> = RefCell::new(
        (0..CONNS)
            .map(|_| rt.block_on(tokio::net::TcpStream::connect(addr)).unwrap())
            .collect(),
    );
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let tasks: Vec<_> = conns
                .borrow_mut()
                .drain(..)
                .map(|mut conn| {
                    rt.spawn(async move {
                        tokio_exchange(&mut conn, 128, PER_CONN).await;
                        conn
                    })
                })
                .collect();
            for task in tasks {
                conns.borrow_mut().push(rt.block_on(task).unwrap());
            }
        })
    });
    g.finish();
}

fn tcp_bulk(c: &mut Criterion) {
    const BULK: usize = 64 << 20;
    const CHUNK: usize = 64 << 10;
    let mut g = group(
        c,
        "tcp_bulk_transfer_64MiB",
        Throughput::Bytes(BULK as u64),
        true,
    );
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let sink = rt.spawn(async move {
                let listener = nuclei::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let receiver = nuclei::runtime::spawn(async move {
                    let (conn, _) = listener.accept().await.unwrap();
                    let mut buf = vec![0u8; CHUNK];
                    let mut total = 0;
                    while total < BULK {
                        total += conn.recv(&mut buf).await.unwrap();
                    }
                    total
                });
                let conn = nuclei::net::TcpStream::connect(addr).await.unwrap();
                let chunk = vec![1u8; CHUNK];
                let mut sent = 0;
                while sent < BULK {
                    sent += conn.send(&chunk[..CHUNK.min(BULK - sent)]).await.unwrap();
                }
                receiver.await
            });
            assert_eq!(rt.block_on(sink), BULK);
        })
    });
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let sink = rt.spawn(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let receiver = tokio::spawn(async move {
                    let (mut conn, _) = listener.accept().await.unwrap();
                    let mut buf = vec![0u8; CHUNK];
                    let mut total = 0;
                    while total < BULK {
                        total += conn.read(&mut buf).await.unwrap();
                    }
                    total
                });
                let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
                let chunk = vec![1u8; CHUNK];
                let mut sent = 0;
                while sent < BULK {
                    let n = CHUNK.min(BULK - sent);
                    conn.write_all(&chunk[..n]).await.unwrap();
                    sent += n;
                }
                receiver.await.unwrap()
            });
            assert_eq!(rt.block_on(sink).unwrap(), BULK);
        })
    });
    g.finish();
}

fn udp(c: &mut Criterion) {
    const ROUNDS: u64 = 2_000;
    let mut g = group(
        c,
        "udp_round_trips_128B",
        Throughput::Elements(ROUNDS),
        true,
    );
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            rt.block_on(rt.spawn(async {
                let a = nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let echo = nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let (a_addr, echo_addr) = (a.local_addr().unwrap(), echo.local_addr().unwrap());
                let server = nuclei::runtime::spawn(async move {
                    let mut buf = [0u8; 256];
                    for _ in 0..ROUNDS {
                        let (n, _) = echo.recv_from(&mut buf).await.unwrap();
                        echo.send_to(&buf[..n], a_addr).await.unwrap();
                    }
                });
                let request = [3u8; 128];
                let mut reply = [0u8; 256];
                for _ in 0..ROUNDS {
                    a.send_to(&request, echo_addr).await.unwrap();
                    a.recv_from(&mut reply).await.unwrap();
                }
                server.await;
            }));
        })
    });
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            rt.block_on(rt.spawn(async {
                let a = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let echo = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let (a_addr, echo_addr) = (a.local_addr().unwrap(), echo.local_addr().unwrap());
                let server = tokio::spawn(async move {
                    let mut buf = [0u8; 256];
                    for _ in 0..ROUNDS {
                        let (n, _) = echo.recv_from(&mut buf).await.unwrap();
                        echo.send_to(&buf[..n], a_addr).await.unwrap();
                    }
                });
                let request = [3u8; 128];
                let mut reply = [0u8; 256];
                for _ in 0..ROUNDS {
                    a.send_to(&request, echo_addr).await.unwrap();
                    a.recv_from(&mut reply).await.unwrap();
                }
                server.await.unwrap();
            }))
            .unwrap();
        })
    });
    g.finish();
}

fn file_read(c: &mut Criterion) {
    const SIZE: usize = 16 << 20;
    const CHUNK: usize = 64 << 10;
    let mut path = std::env::temp_dir();
    path.push(format!("nuclei-vs-tokio-{}", std::process::id()));
    std::fs::write(&path, vec![5u8; SIZE]).unwrap();
    let mut g = group(c, "file_read_16MiB", Throughput::Bytes(SIZE as u64), true);
    let rt = nuclei_rt(1);
    g.bench_function("nuclei", |b| {
        b.iter(|| {
            let path = path.clone();
            let total = rt.block_on(rt.spawn(async move {
                let file = nuclei::fs::File::open(&path).await.unwrap();
                let mut buf = Vec::with_capacity(CHUNK);
                let mut total = 0;
                loop {
                    buf.clear();
                    let (read, back) = file.read_owned(buf).await;
                    buf = back;
                    match read.unwrap() {
                        0 => break total,
                        n => total += n,
                    }
                }
            }));
            assert_eq!(total, SIZE);
        })
    });
    drop(rt);
    let rt = tokio_rt();
    g.bench_function("tokio", |b| {
        b.iter(|| {
            let path = path.clone();
            let total = rt
                .block_on(rt.spawn(async move {
                    let mut file = tokio::fs::File::open(&path).await.unwrap();
                    let mut buf = vec![0u8; CHUNK];
                    let mut total = 0;
                    loop {
                        match file.read(&mut buf).await.unwrap() {
                            0 => break total,
                            n => total += n,
                        }
                    }
                }))
                .unwrap();
            assert_eq!(total, SIZE);
        })
    });
    g.finish();
    drop(rt);
    let _ = std::fs::remove_file(&path);
}

criterion_group!(
    benches,
    block_on_ready,
    spawn_join,
    yield_loop,
    mpsc_ping_pong,
    oneshot_pairs,
    mpsc_across_workers,
    arm_and_fire,
    timeout_hit,
    timeout_miss,
    tcp_ping_pong,
    tcp_256,
    tcp_bulk,
    udp,
    file_read
);
criterion_main!(benches);
