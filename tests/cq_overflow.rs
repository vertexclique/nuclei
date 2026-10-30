//! Many times the completions an io_uring ring's queue holds, ending at once: the kernel holds
//! those past the queue (`IORING_FEAT_NODROP`), and the shard takes every one, so no connection or
//! read is lost and every byte is the one sent. Since no operation of a source counts against the
//! ring's cap, nothing bounds how many end together but the sources. While the shard's thread is
//! held, peers connect to a listener whose accept stream keeps one multishot accept armed and
//! send a request each: once the thread enters the kernel again the accept hands out connections
//! until the queue is full, and its last completion overflows it. A binary of its own, since the
//! configuration is the process's: the smallest ring, with the receive group off, so each read is
//! one receive with one completion. On epoll and kqueue no completion queue exists, and every
//! request is answered the same way.

use core::pin::Pin;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream as StdStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nuclei::config::{IoUringConfiguration, NucleiConfig};
use nuclei::net::{TcpSocket, TcpStream};
use nuclei::runtime::Builder;

/// The server's ring, the least a ring may have: 128 completions.
const QUEUE: u32 = 64;
const COMPLETIONS: usize = 128;
/// Connections, and receives, that all end at once: three times the queue each.
const PEERS: usize = 3 * COMPLETIONS;
const REQUEST: usize = 64;
/// The server's buffers, a cap no other shard of the process has.
const CAP: usize = 8 << 20;

/// The longest the whole burst may take to be answered on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(30);

/// Peer `peer`'s request: bytes no other peer's request has at the same place.
fn request(peer: usize) -> [u8; REQUEST] {
    core::array::from_fn(|i| (peer.wrapping_mul(31) ^ i.wrapping_mul(7)) as u8)
}

/// Receives one request whole and sends it back, then holds the stream until its peer closes.
async fn echo(stream: TcpStream) {
    let mut got = [0u8; REQUEST];
    let mut n = 0;
    while n < REQUEST {
        match stream.recv(&mut got[n..]).await {
            Ok(k @ 1..) => n += k,
            _ => return,
        }
    }
    let mut out = &got[..];
    while !out.is_empty() {
        match stream.send(out).await {
            Ok(k @ 1..) => out = &out[k..],
            _ => return,
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
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = socket.local_addr().unwrap();
    // A backlog that holds every peer while the shard's thread is held.
    let listener = socket.listen(2 * PEERS as u32).unwrap();
    rt.spawn(async move {
        let mut conns = listener.accept_multi().await.unwrap();
        while let Some(stream) =
            core::future::poll_fn(|cx| futures_core::Stream::poll_next(Pin::new(&mut conns), cx))
                .await
        {
            nuclei::runtime::spawn(echo(stream)).detach();
        }
    })
    .detach();
    let ring = nuclei::Proactor::backend() == nuclei::IoBackend::IoUring;
    // On a ring the shard's thread is held in one task, so no completion is taken until every
    // peer connected and sent its request.
    let release = Arc::new(AtomicBool::new(!ring));
    let hold = ring.then(|| {
        until("the accept armed", || server_shard().0 > 0);
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
    let mut peers: Vec<StdStream> = (0..PEERS).map(|_| connect(addr)).collect();
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
            panic!("peer {peer} of {PEERS} got no reply within {DEADLINE:?}: {err}");
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
