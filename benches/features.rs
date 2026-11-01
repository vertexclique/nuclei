//! A sweep of the runtime's features, one row each, for comparing a build
//! with another: `block_on`, tasks passing messages, timers, TCP round trips
//! on one shard and across 256 connections, a bulk TCP transfer, UDP round
//! trips, a file read, channels across shards, and the handoff path.
//!
//! Run with `cargo bench --bench features`; pin it for steady numbers
//! (`taskset -c 2,3 cargo bench --bench features`). Each row's element is
//! named in its group; every runtime has its workers unpinned.

use std::io::Write as _;
use std::net::SocketAddr;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use nuclei::net::{TcpListener, TcpStream, UdpSocket};
use nuclei::runtime::{Builder, Runtime};
use nuclei::sync::{mpsc, oneshot};

fn runtime(workers: usize) -> Runtime {
    Builder::new()
        .worker_threads(workers)
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .expect("the runtime starts")
}

/// A group for rows that run a few milliseconds or more each.
fn slow_group<'a>(
    c: &'a mut Criterion,
    name: &str,
) -> criterion::BenchmarkGroup<'a, criterion::measurement::WallTime> {
    let mut group = c.benchmark_group(name);
    group.sample_size(20);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));
    group
}

fn block_on_and_messages(c: &mut Criterion) {
    let rt = runtime(1);
    let mut group = c.benchmark_group("tasks");
    group.bench_function("block_on_round_trip", |b| {
        b.iter(|| rt.block_on(async { 1u8 }))
    });
    // Two tasks pass a token through a channel of one slot, back and forth.
    const ROUNDS: u64 = 2_000;
    group.throughput(Throughput::Elements(ROUNDS * 2));
    group.bench_function("ping_pong_mpsc", |b| {
        b.iter(|| {
            let (to_b, mut from_a) = mpsc::channel::<u64>(1);
            let (to_a, mut from_b) = mpsc::channel::<u64>(1);
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
        });
    });
    group.finish();
}

fn timers(c: &mut Criterion) {
    let rt = runtime(1);
    let mut group = slow_group(c, "timers");
    const ARMED: u64 = 1_000;
    group.throughput(Throughput::Elements(ARMED));
    group.bench_function("arm_and_fire_1000_sleeps", |b| {
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
        });
    });
    const HITS: u64 = 1_000;
    group.throughput(Throughput::Elements(HITS));
    group.bench_function("timeout_hit", |b| {
        b.iter(|| {
            rt.block_on(rt.spawn(async {
                for _ in 0..HITS {
                    nuclei::time::timeout(Duration::from_secs(1), async {})
                        .await
                        .unwrap();
                }
            }));
        });
    });
    const MISSES: u64 = 20;
    group.throughput(Throughput::Elements(MISSES));
    group.bench_function("timeout_miss_1ms", |b| {
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
        });
    });
    group.finish();
}

/// An echo server on `rt`; returns its address.
fn echo_server(rt: &Runtime) -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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

/// `rounds` request/reply exchanges of `size` bytes on `conn`.
async fn exchange(conn: &TcpStream, size: usize, rounds: u64) {
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

fn tcp(c: &mut Criterion) {
    let rt = runtime(1);
    let addr = echo_server(&rt);
    let mut group = slow_group(c, "tcp");
    const ROUNDS: u64 = 2_000;
    group.throughput(Throughput::Elements(ROUNDS));
    group.bench_function("ping_pong_one_connection", |b| {
        let conn = std::sync::Arc::new(
            rt.block_on(rt.spawn(async move { TcpStream::connect(addr).await.unwrap() })),
        );
        b.iter(|| {
            let conn = std::sync::Arc::clone(&conn);
            rt.block_on(rt.spawn(async move { exchange(&conn, 64, ROUNDS).await }));
        });
    });
    const CONNS: u64 = 256;
    const PER_CONN: u64 = 20;
    group.throughput(Throughput::Elements(CONNS * PER_CONN));
    group.bench_function("request_reply_256_connections", |b| {
        let conns: Vec<_> = (0..CONNS)
            .map(|_| {
                std::sync::Arc::new(
                    rt.block_on(rt.spawn(async move { TcpStream::connect(addr).await.unwrap() })),
                )
            })
            .collect();
        b.iter(|| {
            let tasks: Vec<_> = conns
                .iter()
                .map(|conn| {
                    let conn = std::sync::Arc::clone(conn);
                    rt.spawn(async move { exchange(&conn, 128, PER_CONN).await })
                })
                .collect();
            for task in tasks {
                rt.block_on(task);
            }
        });
    });
    group.finish();

    // Bulk: a sink server counts the bytes of one connection.
    const BULK: usize = 64 << 20;
    const CHUNK: usize = 64 << 10;
    let mut group = slow_group(c, "tcp_bulk");
    group.throughput(Throughput::Bytes(BULK as u64));
    group.bench_function("transfer_64MiB", |b| {
        b.iter(|| {
            let sink = rt.spawn(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
                let conn = TcpStream::connect(addr).await.unwrap();
                let chunk = vec![1u8; CHUNK];
                let mut sent = 0;
                while sent < BULK {
                    sent += conn.send(&chunk[..CHUNK.min(BULK - sent)]).await.unwrap();
                }
                receiver.await
            });
            assert_eq!(rt.block_on(sink), BULK);
        });
    });
    group.finish();
}

fn udp(c: &mut Criterion) {
    let rt = runtime(1);
    let mut group = slow_group(c, "udp");
    const ROUNDS: u64 = 2_000;
    group.throughput(Throughput::Elements(ROUNDS));
    group.bench_function("round_trips_128B", |b| {
        b.iter(|| {
            rt.block_on(rt.spawn(async {
                let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
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
        });
    });
    group.finish();
}

fn file(c: &mut Criterion) {
    const SIZE: usize = 16 << 20;
    const CHUNK: usize = 64 << 10;
    let mut path = std::env::temp_dir();
    path.push(format!("nuclei-features-{}", std::process::id()));
    std::fs::File::create(&path)
        .unwrap()
        .write_all(&vec![5u8; SIZE])
        .unwrap();
    let rt = runtime(1);
    let mut group = slow_group(c, "file");
    group.throughput(Throughput::Bytes(SIZE as u64));
    group.bench_function("read_16MiB", |b| {
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
        });
    });
    group.finish();
    drop(rt);
    let _ = std::fs::remove_file(&path);
}

fn channels(c: &mut Criterion) {
    let rt = runtime(2);
    let mut group = slow_group(c, "channels");
    const MESSAGES: u64 = 20_000;
    group.throughput(Throughput::Elements(MESSAGES));
    group.bench_function("mpsc_across_shards", |b| {
        b.iter(|| {
            let (tx, mut rx) = mpsc::channel::<u64>(256);
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
        });
    });
    const PAIRS: u64 = 2_000;
    group.throughput(Throughput::Elements(PAIRS));
    group.bench_function("oneshot_pairs", |b| {
        b.iter(|| {
            let tasks: Vec<_> = (0..PAIRS)
                .map(|i| {
                    let (tx, rx) = oneshot::channel::<u64>();
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
        });
    });
    group.finish();
}

fn handoff(c: &mut Criterion) {
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(5)))
        .build()
        .expect("the runtime starts");
    let mut group = slow_group(c, "handoff");
    const TASKS: u64 = 100;
    const YIELDS: u64 = 500;
    group.throughput(Throughput::Elements(TASKS * YIELDS));
    group.bench_function("yielders_beside_a_blocked_task", |b| {
        b.iter(|| {
            let blocked = rt.spawn(async {
                std::thread::sleep(Duration::from_millis(30));
            });
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
            rt.block_on(blocked);
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    block_on_and_messages,
    timers,
    tcp,
    udp,
    file,
    channels,
    handoff
);
criterion_main!(benches);
