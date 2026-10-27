//! Nothing leaks: each subsystem's workload, run again and again after a
//! warm-up, leaves the process with no more open descriptors (handles on
//! Windows) and no more live heap bytes than after the warm-up, but for a
//! fixed slack: what other threads retired and have not reclaimed yet (a
//! bounded batch each). A leak of 8 bytes a round passes the slack, since
//! the rounds outnumber it. One test runs every workload in turn, so no
//! other test moves the counts meanwhile.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream, UdpSocket};

struct Live;

static LIVE: AtomicIsize = AtomicIsize::new(0);

fn add(n: usize) {
    LIVE.fetch_add(n as isize, Ordering::Relaxed);
}

fn sub(n: usize) {
    LIVE.fetch_sub(n as isize, Ordering::Relaxed);
}

// SAFETY: every call goes to the system allocator as it came.
unsafe impl GlobalAlloc for Live {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            add(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as above.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            add(layout.size());
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as above.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            sub(layout.size());
            add(new_size);
        }
        new
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        sub(layout.size());
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Live = Live;

/// What may be live more after the rounds than after the warm-up: nodes
/// other threads retired and have not reclaimed yet. Measured at zero on
/// both Linux backends, with this thread's retired nodes flushed.
const SLACK: isize = 16 * 1024;

/// The descriptors this process holds open.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_handles() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn open_handles() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

#[cfg(windows)]
fn open_handles() -> usize {
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessHandleCount(process: isize, count: *mut u32) -> i32;
    }
    let mut count = 0u32;
    // SAFETY: the pseudo handle of this process, and a place for the count.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
    count as usize
}

/// Runs `round` `warm` times, then `rounds` more, and checks that the open
/// handles and live bytes came back to where they stood after the warm-up.
fn steady(name: &str, warm: usize, rounds: usize, mut round: impl FnMut()) {
    for _ in 0..warm {
        round();
    }
    // What this thread retired is freed now, not at the next wave of its
    // reclamation; other threads hold a bounded batch each.
    kovan::flush();
    let (handles, bytes) = (open_handles(), LIVE.load(Ordering::Relaxed));
    for _ in 0..rounds {
        round();
    }
    kovan::flush();
    let (handles_after, bytes_after) = (open_handles(), LIVE.load(Ordering::Relaxed));
    assert!(
        handles_after <= handles,
        "{name}: {handles} open handles after the warm-up, {handles_after} after {rounds} more rounds"
    );
    let grew = bytes_after - bytes;
    assert!(
        grew <= SLACK,
        "{name}: {grew} live bytes more after {rounds} rounds than after the warm-up"
    );
}

async fn tcp_pair(listener: &TcpListener) -> (TcpStream, TcpStream) {
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (client, server)
}

#[test]
fn every_subsystem_holds_steady() {
    let rt = nuclei::Runtime::builder()
        .worker_threads(2)
        .build()
        .unwrap();
    steady("spawns", 200, 5000, || {
        rt.block_on(async {
            let local = nuclei::spawn_local(async { 1u8 });
            let remote = nuclei::spawn(async { 2u8 });
            let blocking = nuclei::spawn_blocking(|| 3u8);
            nuclei::spawn(async {}).detach();
            assert_eq!(local.await + remote.await + blocking.await, 6);
        });
    });

    steady("join sets", 100, 2000, || {
        rt.block_on(async {
            let mut set = nuclei::task::JoinSet::new();
            for i in 0..8u32 {
                set.spawn(async move {
                    for _ in 0..i {
                        nuclei::task::yield_now().await;
                    }
                    i
                });
            }
            for _ in 0..4 {
                set.join_next().await.unwrap().unwrap();
            }
            // The rest are cancelled with the set, some of them waking.
        });
    });

    let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    steady("tcp", 100, 2000, || {
        rt.block_on(async {
            let (mut client, mut server) = tcp_pair(&listener).await;
            client.write_all(&[1u8; 1024]).await.unwrap();
            client.flush().await.unwrap();
            let mut buf = [0u8; 1024];
            server.read_exact(&mut buf).await.unwrap();
            server.write_all(&buf).await.unwrap();
            server.flush().await.unwrap();
            client.read_exact(&mut buf).await.unwrap();
        });
    });

    steady("udp", 100, 2000, || {
        rt.block_on(async {
            let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            a.send_to(b"datagram", b.local_addr().unwrap())
                .await
                .unwrap();
            let mut buf = [0u8; 16];
            let (n, _) = b.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 8);
        });
    });

    #[cfg(unix)]
    steady("unix streams and pipes", 100, 2000, || {
        rt.block_on(async {
            let (mut a, mut b) = nuclei::net::UnixStream::pair().unwrap();
            a.write_all(b"unix").await.unwrap();
            a.flush().await.unwrap();
            let mut buf = [0u8; 4];
            b.read_exact(&mut buf).await.unwrap();
            let (mut tx, mut rx) = nuclei::net::unix::pipe::pipe().unwrap();
            tx.write_all(b"pipe").await.unwrap();
            tx.flush().await.unwrap();
            rx.read_exact(&mut buf).await.unwrap();
        });
    });

    let path = std::env::temp_dir().join(format!("nuclei-steady-{}", std::process::id()));
    steady("files", 100, 2000, || {
        rt.block_on(async {
            nuclei::fs::write(&path, [7u8; 4096]).await.unwrap();
            assert_eq!(nuclei::fs::read(&path).await.unwrap().len(), 4096);
            let mut file = nuclei::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .await
                .unwrap();
            file.write_all(&[8u8; 512]).await.unwrap();
            file.flush().await.unwrap();
        });
    });
    let _ = std::fs::remove_file(&path);

    steady("timers", 100, 2000, || {
        rt.block_on(async {
            nuclei::time::sleep(Duration::from_micros(10)).await;
            let pending = nuclei::time::timeout(Duration::ZERO, core::future::pending::<()>());
            assert!(pending.await.is_err());
            let mut ticks = nuclei::time::interval(Duration::from_micros(10));
            ticks.tick().await;
            ticks.tick().await;
        });
    });

    steady("channels", 200, 5000, || {
        rt.block_on(async {
            let (tx, mut rx) = nuclei::sync::mpsc::channel(4);
            let (utx, mut urx) = nuclei::sync::mpsc::unbounded_channel();
            let (otx, orx) = nuclei::sync::oneshot::channel();
            let (btx, mut brx) = nuclei::sync::broadcast::channel(4);
            nuclei::spawn(async move {
                tx.send(1u32).await.unwrap();
                utx.send(2u32).unwrap();
                otx.send(3u32).unwrap();
                btx.send(4u32).unwrap();
            })
            .detach();
            let sum = rx.recv().await.unwrap()
                + urx.recv().await.unwrap()
                + orx.await.unwrap()
                + brx.recv().await.unwrap();
            assert_eq!(sum, 10);
        });
    });

    steady("child processes", 20, 200, || {
        rt.block_on(async {
            #[cfg(unix)]
            let mut command = nuclei::process::Command::new("true");
            #[cfg(windows)]
            let mut command = {
                let mut command = nuclei::process::Command::new("cmd");
                command.args(["/C", "exit 0"]);
                command
            };
            assert!(command.status().await.unwrap().success());
        });
    });

    #[cfg(unix)]
    steady("signal streams", 100, 2000, || {
        rt.block_on(async {
            let signal =
                nuclei::signal::unix::signal(nuclei::signal::unix::SignalKind::user_defined1());
            drop(signal.unwrap());
        });
    });

    steady("cancelled operations", 100, 2000, || {
        rt.block_on(async {
            let (client, mut server) = tcp_pair(&listener).await;
            let mut buf = [0u8; 64];
            let dropped = nuclei::time::timeout(Duration::ZERO, server.read(&mut buf)).await;
            assert!(dropped.is_err());
            let dropped = nuclei::time::timeout(Duration::ZERO, client.write_owned(vec![5u8; 64]));
            let _ = dropped.await;
            // The peer takes what went out, so nothing lingers behind.
            drop(client);
            let mut rest = Vec::new();
            let _ = server.read_to_end(&mut rest).await;
        });
    });

    #[cfg(unix)]
    steady("caller's own descriptors", 100, 2000, || {
        use std::io::{Read, Write};
        rt.block_on(async {
            let (a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
            a.set_nonblocking(true).unwrap();
            let fd = nuclei::io::unix::AsyncFd::new(a).unwrap();
            b.write_all(b"fd").unwrap();
            let mut buf = [0u8; 2];
            fd.async_io(nuclei::io::Interest::READABLE, |a| (&*a).read(&mut buf))
                .await
                .unwrap();
        });
    });
    drop(listener);
    drop(rt);

    steady("runtimes started and stopped", 10, 100, || {
        let rt = nuclei::Runtime::builder()
            .worker_threads(2)
            .build()
            .unwrap();
        assert_eq!(rt.block_on(async { nuclei::spawn(async { 5u8 }).await }), 5);
        drop(rt);
    });
}
