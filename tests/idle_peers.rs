//! Connections whose peers never send keep no other connection from being accepted, read and
//! answered: on an io_uring ring their receives, each waiting for its peer, may hold the whole cap
//! on operations in the kernel and the whole pool of IO buffers, and still the next accept goes in
//! and a reader or a writer the pool has no buffer for uses its own. The vs-tokio benchmark found
//! the stall at 4096 connections of 16 KiB requests: past 3840 waiting receives, the cap of a ring
//! of 2048 entries beside its group, the next accept waited for good. A binary of its own, since
//! the configuration is the process's: the smallest ring, with the receive group off, so every
//! read below 16 KiB is a receive into a buffer of the pool that waits. On epoll and kqueue no
//! receive waits in the kernel, and every request is answered the same way.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream as StdStream};
use std::time::{Duration, Instant};

use nuclei::config::{IoUringConfiguration, NucleiConfig};
use nuclei::net::{TcpListener, TcpStream};
use nuclei::runtime::Builder;

/// The server's ring, the least a ring may have: 128 completions, the cap on operations in the
/// kernel with no receive group.
const QUEUE: u32 = 64;
/// The cap of the server's buffers, and each read's buffer: one size class, below what a ring
/// reads straight into a task's buffer.
const CAP: usize = 1 << 20;
const READ: usize = 4096;
/// Peers that never send, more than the cap on operations and the pool hold, and peers that send
/// a request and wait for its reply.
const IDLE: usize = CAP / READ + 16;
const ACTIVE: usize = 16;
const REPLY: usize = 64;

/// The longest a reply may take on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(30);

/// Peeks at what came before it reads, so a peek takes the same way as a read.
async fn serve(stream: TcpStream) {
    let mut buf = vec![0u8; READ];
    let mut pending = 0;
    while let Ok(1..) = stream.peek(&mut buf).await
        && let Ok(n @ 1..) = stream.recv(&mut buf).await
    {
        pending += n;
        while pending >= READ {
            pending -= READ;
            let mut reply = &[7u8; REPLY][..];
            while !reply.is_empty() {
                match stream.send(reply).await {
                    Ok(n) => reply = &reply[n..],
                    Err(_) => return,
                }
            }
        }
    }
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

#[test]
fn idle_peers_holding_the_cap_and_the_pool_keep_no_request_from_its_reply() {
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
        .io_buffer_cap(CAP)
        .build()
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    rt.spawn(async move {
        let listener = TcpListener::new(listener).unwrap();
        while let Ok((stream, _)) = listener.accept().await {
            nuclei::runtime::spawn(serve(stream)).detach();
        }
    })
    .detach();
    let idle: Vec<StdStream> = (0..IDLE).map(|_| connect(addr)).collect();
    if nuclei::Proactor::backend() == nuclei::IoBackend::IoUring {
        // Every buffer of the pool, and more than the cap on operations, wait in receives of idle
        // peers, and a reader found no buffer.
        until("the pool full of waiting receives", || {
            nuclei::stats()
                .shards
                .iter()
                .any(|s| s.buffer_cap == CAP && s.buffer_exhausted > 0)
        });
    }
    let mut active: Vec<StdStream> = (0..ACTIVE).map(|_| connect(addr)).collect();
    for stream in &mut active {
        stream.write_all(&[3u8; READ]).unwrap();
    }
    for (i, stream) in active.iter_mut().enumerate() {
        let mut reply = [0u8; REPLY];
        if let Err(err) = stream.read_exact(&mut reply) {
            panic!("peer {i} of {ACTIVE} got no reply within {DEADLINE:?}: {err}");
        }
        assert_eq!(reply, [7u8; REPLY]);
    }
    drop(idle);
}
