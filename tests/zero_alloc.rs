//! The operations that allocate nothing in steady state: a global allocator
//! counts each thread's allocations, and a run of operations on a runtime
//! with no workers (everything on this thread) must make none once warm.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
}

// SAFETY: every call goes to the system allocator as it came.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

const WARM: usize = 1_000;
const RUNS: usize = 10_000;

/// Allocations this thread made in `RUNS` rounds of `round`, after `WARM`.
fn steady<F: AsyncFnMut()>(mut round: F) -> u64 {
    let rt = nuclei::Runtime::builder()
        .worker_threads(0)
        .build()
        .unwrap();
    rt.block_on(async {
        for _ in 0..WARM {
            round().await;
        }
        let before = allocations();
        for _ in 0..RUNS {
            round().await;
        }
        allocations() - before
    })
}

#[test]
fn udp_datagrams_to_an_address_allocate_nothing() {
    let rt = nuclei::Runtime::builder()
        .worker_threads(0)
        .build()
        .unwrap();
    let (a, b) = rt.block_on(async {
        (
            nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        )
    });
    drop(rt);
    let to = b.local_addr().unwrap();
    let mut buf = [0u8; 64];
    let made = steady(async || {
        a.send_to(b"datagram", to).await.unwrap();
        b.recv_from(&mut buf).await.unwrap();
    });
    assert_eq!(
        made, 0,
        "{made} allocations in {RUNS} datagrams sent and received"
    );
}

#[test]
fn connected_udp_allocates_nothing() {
    let rt = nuclei::Runtime::builder()
        .worker_threads(0)
        .build()
        .unwrap();
    let (a, b) = rt.block_on(async {
        let a = nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        a.connect(b.local_addr().unwrap()).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        (a, b)
    });
    drop(rt);
    let mut buf = [0u8; 64];
    let made = steady(async || {
        a.send(b"datagram").await.unwrap();
        b.recv(&mut buf).await.unwrap();
    });
    assert_eq!(
        made, 0,
        "{made} allocations in {RUNS} datagrams sent and received"
    );
}

#[test]
fn a_tcp_round_trip_allocates_nothing() {
    let rt = nuclei::Runtime::builder()
        .worker_threads(0)
        .build()
        .unwrap();
    let (client, server) = rt.block_on(async {
        let listener = nuclei::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = nuclei::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    });
    drop(rt);
    let mut buf = [0u8; 64];
    let made = steady(async || {
        client.send(b"ping").await.unwrap();
        let mut got = 0;
        while got < 4 {
            got += server.recv(&mut buf[got..4]).await.unwrap();
        }
    });
    assert_eq!(made, 0, "{made} allocations in {RUNS} round trips");
}

/// File reads and writes through a handle: measured, not yet held to zero
/// (N-ZERO-4: a ring operation's slot, a pool job's task).
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn file_reads_and_writes_measured() {
    use nuclei::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    let path = std::env::temp_dir().join(format!("nuclei-zero-alloc-{}", std::process::id()));
    let rt = nuclei::Runtime::builder()
        .worker_threads(0)
        .build()
        .unwrap();
    let file = rt.block_on(async {
        nuclei::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await
            .unwrap()
    });
    drop(rt);
    let mut file = file;
    let mut buf = [0u8; 4096];
    let made = steady(async || {
        file.seek(std::io::SeekFrom::Start(0)).await.unwrap();
        file.write_all(&[7u8; 4096]).await.unwrap();
        file.flush().await.unwrap();
        file.seek(std::io::SeekFrom::Start(0)).await.unwrap();
        file.read_exact(&mut buf).await.unwrap();
    });
    eprintln!("FILE: {made} allocations in {RUNS} write, flush, seek and read rounds");
    let _ = std::fs::remove_file(&path);
}

/// A task's memory comes from the thread's cache of task blocks.
#[test]
fn a_local_spawn_allocates_nothing() {
    let made = steady(async || {
        nuclei::spawn_local(async { 1u8 }).await;
    });
    assert_eq!(made, 0, "{made} allocations in {RUNS} spawns and awaits");
}

/// Spawns through queues to other threads: measured, not yet held to zero
/// (N-ZERO-7: the blocking pool's channel takes a node per job, a shard's
/// inbox a segment per 32 tasks from other threads).
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn spawns_to_other_threads_measured() {
    let blocking = steady(async || {
        nuclei::spawn_blocking(|| 1u8).await;
    });
    eprintln!("SPAWN_BLOCKING: {blocking} allocations in {RUNS} spawn_blocking and await");
    // Onto a worker, from this thread: counted on this thread only.
    let rt = nuclei::Runtime::builder()
        .worker_threads(1)
        .build()
        .unwrap();
    let remote = rt.block_on(async {
        for _ in 0..WARM {
            nuclei::spawn(async { 1u8 }).await;
        }
        let before = allocations();
        for _ in 0..RUNS {
            nuclei::spawn(async { 1u8 }).await;
        }
        allocations() - before
    });
    eprintln!(
        "SPAWN_REMOTE: {remote} allocations on the spawning thread in {RUNS} spawn and await"
    );
}
