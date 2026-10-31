//! Connections whose peers never send, peers that never read, and tasks that stop reading keep no
//! other connection from being accepted, read and answered: on an io_uring ring the receives of
//! idle peers, each waiting for its peer, may hold the whole cap on operations in the kernel and
//! the whole pool of IO buffers, and still the next accept goes in and a reader or a writer the
//! pool has no buffer for uses its own; sends that wait for peers that never read count against
//! no cap either. The vs-tokio benchmark found the stall at 4096 connections of 16 KiB requests:
//! past 3840 waiting receives, the cap of a ring of 2048 entries beside its group, the next accept
//! waited for good. On every backend bytes read ahead for tasks that peeked and stopped may hold
//! the whole pool too, and a reader the pool has no buffer for still reads into its own. A binary
//! of its own, since the configuration is the process's: the smallest ring, with the receive group
//! off, so every read below 16 KiB is a receive into a buffer of the pool that waits. On epoll and
//! kqueue no receive waits in the kernel, and every request is answered the same way. On a
//! completion port a read the pool has no buffer for waits in a zero-byte receive, which holds
//! none, and a reader or a writer it has no buffer for uses its own memory
//! (proofs/tla/port/PortStarve.tla). The same holds for datagram sockets nobody sends to and for
//! pipes nobody writes to, on every backend.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream as StdStream};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use nuclei::config::{IoUringConfiguration, NucleiConfig};
use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream, UdpSocket};
use nuclei::runtime::Builder;

/// The server's ring, the least a ring may have: 128 completions, the cap on operations in the
/// kernel with no receive group.
const QUEUE: u32 = 64;
/// The cap of the server's buffers, and each read's buffer: one size class, below what a ring
/// reads straight into a task's buffer.
const CAP: usize = 512 << 10;
const READ: usize = 4096;
/// Peers that never send, more than the cap on operations and the pool hold, and peers that send
/// a request and wait for its reply.
const IDLE: usize = CAP / READ + 16;
const ACTIVE: usize = 16;
const REPLY: usize = 64;

/// The longest a reply may take on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(30);

/// The first byte of a request whose task peeks at it and then never reads it.
const HOLD: u8 = 1;
/// The cap of the buffers such tasks (and idle datagram sockets and pipes) hold, a few of each
/// read's size, and more of them.
const HELD_CAP: usize = 64 << 10;
const HOLDERS: usize = HELD_CAP / READ + 4;

/// The first byte of a request whose reply is a stream its peer never reads.
const FLOOD: u8 = 2;
/// Peers that never read, more than the cap on operations in the kernel, and the cap of the
/// buffers their sends' bytes wait in.
const DEAF: usize = 128 + 16;
const DEAF_CAP: usize = 64 << 20;

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

/// Peeks at a request that starts with `HOLD` and then waits past the test's end, its bytes read
/// ahead; serves any other.
async fn serve_or_hold(stream: TcpStream) {
    let mut buf = vec![0u8; READ];
    if let Ok(1..) = stream.peek(&mut buf).await
        && buf[0] == HOLD
    {
        nuclei::time::sleep(4 * DEADLINE).await;
    }
    serve(stream).await;
}

/// Peeks at a request that starts with `FLOOD` and answers it with a stream its peer never reads,
/// until a send waits for good; serves any other.
async fn serve_or_flood(stream: TcpStream) {
    let mut buf = vec![0u8; READ];
    if let Ok(1..) = stream.peek(&mut buf).await
        && buf[0] == FLOOD
    {
        let chunk = vec![9u8; 64 << 10];
        while stream.send(&chunk).await.is_ok() {}
        return;
    }
    serve(stream).await;
}

/// The smallest ring, with no receive group, for every test of the binary.
fn configure() {
    static CONFIGURED: OnceLock<()> = OnceLock::new();
    CONFIGURED.get_or_init(|| {
        nuclei::Proactor::with_config(NucleiConfig {
            iouring: IoUringConfiguration {
                recv_buffers: 0,
                ..IoUringConfiguration::interrupt_driven(QUEUE)
            },
            ..NucleiConfig::default()
        });
    });
}

/// A runtime of one worker whose buffers may hold `cap` bytes.
fn runtime(cap: usize) -> nuclei::runtime::Runtime {
    configure();
    Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .io_buffer_cap(cap)
        .build()
        .unwrap()
}

/// A server of one worker whose buffers may hold `cap` bytes, serving each connection with `serve`.
fn server<F>(cap: usize, serve: fn(TcpStream) -> F) -> (nuclei::runtime::Runtime, SocketAddr)
where
    F: Future<Output = ()> + Send + 'static,
{
    let rt = runtime(cap);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    rt.spawn(async move {
        let listener = TcpListener::new(listener).unwrap();
        while let Ok((stream, _)) = listener.accept().await {
            nuclei::runtime::spawn(serve(stream)).detach();
        }
    })
    .detach();
    (rt, addr)
}

/// Holds the binary's other tests off while one runs, so the descriptors its peers hold (the
/// first test's more than 500) stay within a 1024-descriptor limit, as a virtual machine of the
/// kernel matrix sets, however many tests run at once.
fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A test that failed holding it leaves it poisoned, and the next still runs alone.
    ONE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether a shard of the buffer cap `cap` found every byte under it in use.
fn exhausted(cap: usize) -> bool {
    nuclei::stats()
        .shards
        .iter()
        .any(|s| s.buffer_cap == cap && s.buffer_exhausted > 0)
}

/// Sends a request from each of `ACTIVE` new peers and checks each is answered.
fn answered(addr: SocketAddr) {
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
    let _alone = one_at_a_time();
    let (_rt, addr) = server(CAP, serve);
    let idle: Vec<StdStream> = (0..IDLE).map(|_| connect(addr)).collect();
    if nuclei::Proactor::backend() == nuclei::IoBackend::IoUring {
        // Every buffer of the pool, and more than the cap on operations, wait in receives of idle
        // peers, and a reader found no buffer.
        until("the pool full of waiting receives", || exhausted(CAP));
    }
    answered(addr);
    drop(idle);
}

/// On every backend: the pool full of bytes read ahead for tasks that peeked and stopped, a reader
/// it has no buffer for reads, and peeks, into its own.
#[test]
fn tasks_that_stop_reading_with_the_pool_full_keep_no_request_from_its_reply() {
    let _alone = one_at_a_time();
    let (_rt, addr) = server(HELD_CAP, serve_or_hold);
    let mut held: Vec<StdStream> = (0..HOLDERS).map(|_| connect(addr)).collect();
    for stream in &mut held {
        stream.write_all(&[HOLD; READ]).unwrap();
    }
    until("the pool full of bytes no task reads", || {
        exhausted(HELD_CAP)
    });
    answered(addr);
    drop(held);
}

/// Sends waiting in the kernel for peers that never read, more than the cap on operations, keep no
/// other connection's reply from going out.
#[test]
fn sends_waiting_for_peers_that_never_read_keep_no_request_from_its_reply() {
    let _alone = one_at_a_time();
    let (_rt, addr) = server(DEAF_CAP, serve_or_flood);
    let mut deaf: Vec<StdStream> = (0..DEAF).map(|_| connect(addr)).collect();
    for stream in &mut deaf {
        stream.write_all(&[FLOOD; READ]).unwrap();
    }
    if nuclei::Proactor::backend() == nuclei::IoBackend::IoUring {
        // A send of each, and the accept, out.
        until("a send out for every peer that never reads", || {
            nuclei::stats()
                .shards
                .iter()
                .any(|s| s.buffer_cap == DEAF_CAP && s.ops_in_flight > DEAF)
        });
    }
    answered(addr);
    drop(deaf);
}

/// Datagram sockets nobody sends to, each with a receive the pool holds a buffer for, keep no
/// datagram from its reply: on a completion port a receive the pool has no buffer for looks with a
/// zero-byte receive that leaves the datagram queued, then takes it into its task's own buffer and
/// sends the reply from there (proofs/tla/port/PortStarve.tla). Elsewhere a datagram is its task's
/// own to receive and send, or the ring's starved rule applies.
#[test]
fn idle_datagram_sockets_holding_the_cap_keep_no_datagram_from_its_reply() {
    let _alone = one_at_a_time();
    let rt = runtime(HELD_CAP);
    for _ in 0..HOLDERS {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        rt.spawn(async move {
            let socket = UdpSocket::new(socket).unwrap();
            let mut buf = vec![0u8; READ];
            let _ = socket.recv_from(&mut buf).await;
        })
        .detach();
    }
    if nuclei::Proactor::backend() == nuclei::IoBackend::Iocp {
        until("the pool full of datagram receives", || exhausted(HELD_CAP));
    }
    let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    client.set_read_timeout(Some(DEADLINE)).unwrap();
    let mut servers = Vec::new();
    for _ in 0..ACTIVE {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        servers.push(socket.local_addr().unwrap());
        rt.spawn(async move {
            let socket = UdpSocket::new(socket).unwrap();
            let mut buf = vec![0u8; READ];
            if let Ok((_, from)) = socket.recv_from(&mut buf).await {
                let _ = socket.send_to(&[7u8; REPLY], from).await;
            }
        })
        .detach();
    }
    for addr in &servers {
        client.send_to(&[3u8; READ], addr).unwrap();
    }
    for i in 0..ACTIVE {
        let mut reply = [0u8; READ];
        match client.recv_from(&mut reply) {
            Ok((n, _)) => assert_eq!(&reply[..n], &[7u8; REPLY]),
            Err(err) => panic!("reply {i} of {ACTIVE} not within {DEADLINE:?}: {err}"),
        }
    }
}

/// A pipe's end as a file nuclei reads or writes as the byte stream it is.
#[cfg(unix)]
fn pipe_file(end: impl Into<std::os::fd::OwnedFd>) -> nuclei::fs::File {
    nuclei::fs::File::new(std::fs::File::from(end.into())).unwrap()
}

#[cfg(windows)]
fn pipe_file(end: impl Into<std::os::windows::io::OwnedHandle>) -> nuclei::fs::File {
    nuclei::fs::File::new(std::fs::File::from(end.into())).unwrap()
}

/// Pipes nobody writes to, each with a read the pool holds a buffer for, keep no request sent down
/// another pipe from its reply: a read or a write the pool has no buffer for uses memory of its own
/// (on a completion port its slot's own room, proofs/tla/port/PortStarve.tla; elsewhere the task's
/// own buffer).
#[test]
fn idle_pipes_holding_the_cap_keep_no_request_from_its_reply() {
    let _alone = one_at_a_time();
    let rt = runtime(HELD_CAP);
    let mut idle = Vec::new();
    for _ in 0..HOLDERS {
        let (r, w) = std::io::pipe().unwrap();
        rt.spawn(async move {
            let reader = pipe_file(r);
            let mut buf = vec![0u8; READ];
            let _ = (&reader).read(&mut buf).await;
        })
        .detach();
        idle.push(w);
    }
    if nuclei::Proactor::backend() == nuclei::IoBackend::Iocp {
        until("the pool full of reads of idle pipes", || {
            exhausted(HELD_CAP)
        });
    }
    let mut replies = Vec::new();
    for _ in 0..ACTIVE {
        let (request, mut send) = std::io::pipe().unwrap();
        let (mut answer, reply) = std::io::pipe().unwrap();
        rt.spawn(async move {
            let (reader, writer) = (pipe_file(request), pipe_file(reply));
            let mut buf = vec![0u8; READ];
            let mut got = 0;
            while got < READ {
                match (&reader).read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => got += n,
                }
            }
            let _ = (&writer).write_all(&[7u8; REPLY]).await;
            let _ = (&writer).flush().await;
        })
        .detach();
        send.write_all(&[3u8; READ]).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reply = [0u8; REPLY];
            let _ = tx.send(answer.read_exact(&mut reply).map(|()| reply));
        });
        replies.push((send, rx));
    }
    for (i, (_, rx)) in replies.iter().enumerate() {
        match rx.recv_timeout(DEADLINE) {
            Ok(Ok(reply)) => assert_eq!(reply, [7u8; REPLY]),
            Ok(Err(err)) => panic!("pipe {i} of {ACTIVE}: {err}"),
            Err(_) => panic!("pipe {i} of {ACTIVE} got no reply within {DEADLINE:?}"),
        }
    }
    drop(idle);
}
