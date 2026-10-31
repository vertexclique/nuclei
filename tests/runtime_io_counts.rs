//! The IO operation counts and latencies a runtime's worker reports in
//! `Runtime::stats`: a TCP echo and a UDP round trip counted per worker when
//! the runtime counts, none when it does not, and the latency histograms
//! only on the completion backends that record them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nuclei::IoBackend;
use nuclei::net::{TcpListener, TcpStream, UdpSocket};
use nuclei::runtime::Runtime;
use nuclei::stats::{IoCounts, ShardStats};

const SAFETY: Duration = Duration::from_secs(30);
const ECHOED: usize = 256 * 1024;

fn runtime(count: bool, record: bool) -> Runtime {
    Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .count_io(count)
        .record_latencies(record)
        .build()
        .expect("the runtime starts")
}

fn io(shard: &ShardStats) -> IoCounts {
    shard
        .io
        .unwrap_or_else(|| panic!("a counting runtime's worker has no counts: {shard:?}"))
}

fn settled(rt: &Runtime, done: impl Fn(&ShardStats) -> bool) -> ShardStats {
    let deadline = Instant::now() + SAFETY;
    let mut backoff = Duration::from_millis(1);
    loop {
        let shard = rt.stats().shards.swap_remove(0);
        if done(&shard) {
            return shard;
        }
        assert!(
            Instant::now() < deadline,
            "the counts never settled: {shard:?}"
        );
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(50));
    }
}

async fn send_all(stream: &TcpStream, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let n = stream.send(bytes).await.unwrap();
        bytes = &bytes[n..];
    }
}

async fn echo() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Arc::new(
        TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap(),
    );
    let (server, _) = listener.accept().await.unwrap();
    let server = nuclei::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        let mut seen = 0;
        while seen < ECHOED {
            let n = server.recv(&mut buf).await.unwrap();
            assert_ne!(n, 0);
            send_all(&server, &buf[..n]).await;
            seen += n;
        }
    });
    let writer = {
        let client = Arc::clone(&client);
        nuclei::spawn(async move { send_all(&client, &vec![7u8; ECHOED]).await })
    };
    let mut buf = vec![0u8; 16 * 1024];
    let mut back = 0;
    while back < ECHOED {
        let n = client.recv(&mut buf).await.unwrap();
        assert_ne!(n, 0);
        back += n;
    }
    writer.await;
    server.await;
}

async fn round_trip() {
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    a.send_to(b"ping", b.local_addr().unwrap()).await.unwrap();
    let mut buf = [0u8; 16];
    let (n, from) = b.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping");
    b.send_to(b"pong", from).await.unwrap();
    let (n, _) = a.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"pong");
}

#[test]
fn a_tcp_echo_is_counted_on_its_worker() {
    let rt = runtime(true, false);
    rt.block_on(rt.spawn(echo()));
    let n = ECHOED as u64;
    let shard = settled(&rt, |s| {
        let io = io(s);
        io.bytes_written >= n && io.bytes_read >= n && io.accepts >= 1
    });
    let io = io(&shard);
    assert!(io.reads >= 1 && io.writes >= 1, "{shard:?}");
    assert_eq!(io.accepts, 1, "{shard:?}");
}

#[test]
fn a_udp_round_trip_is_counted_on_its_worker() {
    let rt = runtime(true, false);
    rt.block_on(rt.spawn(round_trip()));
    let shard = settled(&rt, |s| io(s).reads >= 2 && io(s).writes >= 2);
    let io = io(&shard);
    assert!(io.bytes_read >= 8 && io.bytes_written >= 8, "{shard:?}");
}

#[test]
fn latencies_are_kept_only_on_the_completion_backends_that_record_them() {
    let rt = runtime(true, true);
    rt.block_on(rt.spawn(echo()));
    let shard = settled(&rt, |s| io(s).bytes_read >= ECHOED as u64);
    match shard.backend {
        IoBackend::IoUring | IoBackend::Iocp => {
            let times = shard
                .io_latencies
                .as_ref()
                .unwrap_or_else(|| panic!("no latencies on a completion backend: {shard:?}"));
            let timed = times.reads.count()
                + times.writes.count()
                + times.accepts.count()
                + times.connects.count();
            assert!(timed > 0, "{shard:?}");
        }
        IoBackend::Epoll | IoBackend::Kqueue => assert!(shard.io_latencies.is_none(), "{shard:?}"),
    }
}

#[test]
fn latencies_are_none_unless_the_runtime_records_them() {
    let rt = runtime(true, false);
    rt.block_on(rt.spawn(echo()));
    let shard = settled(&rt, |s| io(s).bytes_read >= ECHOED as u64);
    assert!(shard.io_latencies.is_none(), "{shard:?}");
}

#[test]
fn a_runtime_that_does_not_count_reports_no_counts() {
    let rt = runtime(false, false);
    rt.block_on(rt.spawn(echo()));
    assert!(rt.stats().shards.iter().all(|s| s.io.is_none()));
}
