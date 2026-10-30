//! Many times the completions an io_uring ring's queue holds, ending at once: the kernel holds
//! those past the queue (`IORING_FEAT_NODROP`), and the shard takes every one, so no connection or
//! read is lost and every byte is the one sent. Since no operation of a source counts against the
//! ring's cap, nothing bounds how many end together but the sources. While the shard's thread is
//! held, peers connect to listeners whose accept streams each keep one multishot accept armed and
//! send a request each: once the thread enters the kernel again the accepts hand out connections
//! until the queue is full, and their last completions overflow it. The peers are spread over
//! several listeners, so no listener's backlog needs more than the 128 connections every system
//! lets one hold (a peer past its backlog may be reset). A binary of its own, since the
//! configuration is the process's: the smallest ring, with the receive group off, so each read is
//! one receive with one completion. On epoll and kqueue no completion queue exists, and every
//! request is answered the same way.

use core::pin::Pin;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream as StdStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use nuclei::config::{IoUringConfiguration, NucleiConfig};
use nuclei::net::{TcpSocket, TcpStream};
use nuclei::runtime::Builder;

/// The server's ring, the least a ring may have: 128 completions.
const QUEUE: u32 = 64;
const COMPLETIONS: usize = 128;
/// Connections, and receives, that all end at once: three times the queue each, over listeners
/// that each hold fewer than 128 in their backlog.
const PEERS: usize = 3 * COMPLETIONS;
const LISTENERS: usize = 4;
const REQUEST: usize = 64;
/// The server's buffers, a cap no other shard of the process has.
const CAP: usize = 8 << 20;

/// The longest the whole burst may take to be answered on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(30);

/// Peer `peer`'s request: bytes no other peer's request has at the same place.
fn request(peer: usize) -> [u8; REQUEST] {
    core::array::from_fn(|i| (peer.wrapping_mul(31) ^ i.wrapping_mul(7)) as u8)
}

/// The first way a server's connection failed, and how many did.
static FAILED: OnceLock<String> = OnceLock::new();
static FAILURES: AtomicUsize = AtomicUsize::new(0);

fn failed(what: &str, result: std::io::Result<usize>) {
    FAILURES.fetch_add(1, Ordering::SeqCst);
    let _ = FAILED.set(format!("{what}: {result:?}"));
}

/// Receives one request whole and sends it back, then holds the stream until its peer closes.
async fn echo(stream: TcpStream) {
    let mut got = [0u8; REQUEST];
    let mut n = 0;
    while n < REQUEST {
        match stream.recv(&mut got[n..]).await {
            Ok(k @ 1..) => n += k,
            other => return failed("a receive", other),
        }
    }
    let mut out = &got[..];
    while !out.is_empty() {
        match stream.send(out).await {
            Ok(k @ 1..) => out = &out[k..],
            other => return failed("a send", other),
        }
    }
    let _ = stream.recv(&mut got).await;
}

/// Waits until `cond` holds, backing off from 1 ms to 50 ms between looks.
fn until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    let mut pause = Duration::from_millis(1);
    while !cond() {
        assert!(Instant::now() < deadline, "never {what}");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(50));
    }
}

fn connect(addr: SocketAddr) -> StdStream {
    let stream = StdStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(DEADLINE)).unwrap();
    stream
}

/// The published counters of the server's shard: operations out and completion-queue overflows.
fn server_shard() -> (usize, u64) {
    nuclei::stats()
        .shards
        .iter()
        .find(|s| s.buffer_cap == CAP)
        .map_or((0, 0), |s| (s.ops_in_flight, s.cq_overflows))
}

#[test]
fn completions_past_the_queue_all_come_with_their_bytes() {
    nuclei::Proactor::with_config(NucleiConfig {
        iouring: IoUringConfiguration {
            recv_buffers: 0,
            ..IoUringConfiguration::interrupt_driven(QUEUE)
        },
        ..NucleiConfig::default()
    });
    let rt = Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(None)
        .io_buffer_cap(CAP)
        .build()
        .unwrap();
    let ended = Arc::new(OnceLock::new());
    let addrs: Vec<SocketAddr> = (0..LISTENERS)
        .map(|_| {
            let socket = TcpSocket::new_v4().unwrap();
            socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = socket.local_addr().unwrap();
            let listener = socket.listen(127).unwrap();
            let end = Arc::clone(&ended);
            rt.spawn(async move {
                let mut conns = listener.accept_multi().await.unwrap();
                let mut accepted = 0;
                while let Some(stream) = core::future::poll_fn(|cx| {
                    futures_core::Stream::poll_next(Pin::new(&mut conns), cx)
                })
                .await
                {
                    accepted += 1;
                    nuclei::runtime::spawn(echo(stream)).detach();
                }
                let _ = end.set(format!(
                    "ended after {accepted} connections: {:?}",
                    conns.take_error()
                ));
            })
            .detach();
            addr
        })
        .collect();
    let ring = nuclei::Proactor::backend() == nuclei::IoBackend::IoUring;
    // On a ring the shard's thread is held in one task, so no completion is taken until every
    // peer connected and sent its request.
    let release = Arc::new(AtomicBool::new(!ring));
    let hold = ring.then(|| {
        until("the accepts armed", || server_shard().0 >= LISTENERS);
        let holding = Arc::new(AtomicBool::new(false));
        let (held, go) = (Arc::clone(&holding), Arc::clone(&release));
        let hold = rt.spawn(async move {
            held.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + DEADLINE;
            while !go.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        until("the shard held", || holding.load(Ordering::SeqCst));
        hold
    });
    let mut peers: Vec<StdStream> = (0..PEERS)
        .map(|peer| connect(addrs[peer % LISTENERS]))
        .collect();
    for (peer, stream) in peers.iter_mut().enumerate() {
        stream.write_all(&request(peer)).unwrap();
    }
    release.store(true, Ordering::SeqCst);
    if let Some(hold) = hold {
        rt.block_on(hold);
    }
    for (peer, stream) in peers.iter_mut().enumerate() {
        let mut back = [0u8; REQUEST];
        if let Err(err) = stream.read_exact(&mut back) {
            panic!(
                "peer {peer} of {PEERS} got no reply within {DEADLINE:?}: {err}; the accept stream {}; \
                 {} server connections failed, first {}",
                ended.get().map_or("still open", String::as_str),
                FAILURES.load(Ordering::SeqCst),
                FAILED.get().map_or("none", String::as_str)
            );
        }
        assert_eq!(back, request(peer), "peer {peer} got another's bytes");
    }
    if ring {
        until("the queue overflowed and the shard took it all in", || {
            server_shard().1 > 0
        });
    }
    drop(peers);
}
