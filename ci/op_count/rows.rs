//! The rows of the op-count gate: each a fixed amount of work per iteration on a nuclei
//! runtime, shaped as the `nuclei_run` rows of the comparison bench (one worker, unpinned,
//! no handoff; `tcp_2w` two workers; `tcp_watched` the `tcp` row with the handoff watch on).
//! `run(row, n)` does `n` iterations of the row and returns the operations it counted.
//! Included with `#[path]` by any program that wants them.

use std::time::Duration;

const MANY_CONNS: u64 = 256;
const BULK_CHUNK: usize = 64 << 10;
/// The bulk row's socket buffers, each way, as asked of the system.
const BULK_BUFFER: u32 = 4 << 20;
const FILE_CHUNK: usize = 64 << 10;
const SLEEP_TASKS: u64 = 1_000;

/// Every row, in the order the gate reports them.
pub const ROWS: &[&str] = &[
    "block_on",
    "mpsc",
    "timeout",
    "tcp",
    #[cfg(unix)]
    "unix_dgram",
    "udp",
    #[cfg(unix)]
    "unix_stream",
    "tcp_split",
    "tcp_asyncrw",
    "tcp_many",
    "tcp_2w",
    "tcp_watched",
    "bulk",
    "file_read",
    "spawn_join",
    "spawn_blocking",
    "sleep",
    "oneshot",
    "local_mpsc",
    "local_oneshot",
];

#[cfg(unix)]
fn tmp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("nuclei-op-count-{}-{tag}", std::process::id()))
}

/// The file row's file of `size` bytes of data, made once: beside the program, under the
/// build's target directory, on the disk the build is on. Not the system's temporary
/// directory, which may be memory (tmpfs), where reads take other paths than a database's
/// do; a file on a memory filesystem is refused, so such a directory never decides the row.
fn bench_file(size: usize) -> std::path::PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| refuse("the program's directory is unknown"));
    let path = dir.join(format!("nuclei-op-count-file-{size}"));
    if std::fs::metadata(&path).map_or(true, |m| m.len() != size as u64) {
        let data: Vec<u8> = (0..size).map(|i| (i * 7 + i / 251) as u8).collect();
        std::fs::write(&path, data).unwrap_or_else(|err| refuse(&format!("{err}")));
    }
    #[cfg(target_os = "linux")]
    if on_memory(&path) {
        refuse(
            "the file row's file is on a memory filesystem; build into a target directory on a disk",
        );
    }
    path
}

/// Whether `path` lies on a filesystem in memory (tmpfs, ramfs).
#[cfg(target_os = "linux")]
fn on_memory(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    const RAMFS_MAGIC: i64 = 0x8584_58f6;
    let Ok(name) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: plain data; the call fills it for the path named.
    let mut fs: libc::statfs = unsafe { core::mem::zeroed() };
    // SAFETY: `name` is a C string and `fs` is the call's to fill.
    if unsafe { libc::statfs(name.as_ptr(), &mut fs) } != 0 {
        return false;
    }
    matches!(fs.f_type as u32 as i64, TMPFS_MAGIC | RAMFS_MAGIC)
}

/// Ends the program with `why`, as an unusable setup.
fn refuse(why: &str) -> ! {
    eprintln!("op_count: {why}");
    std::process::exit(2);
}

type NTcp = nuclei::net::TcpStream;

async fn n_echo(conn: NTcp) {
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
}

async fn n_rounds(conn: &NTcp, rounds: u64) {
    let request = [7u8; 64];
    let mut reply = [0u8; 64];
    for _ in 0..rounds {
        let mut at = 0;
        while at < 64 {
            at += conn.send(&request[at..]).await.unwrap();
        }
        at = 0;
        while at < 64 {
            at += conn.recv(&mut reply[at..]).await.unwrap();
        }
    }
}

async fn n_listen_echo(accepts: u64) -> std::net::SocketAddr {
    let (tx, rx) = nuclei::sync::oneshot::channel();
    nuclei::spawn(async move {
        let listener = nuclei::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        tx.send(listener.local_addr().unwrap()).unwrap();
        for _ in 0..accepts {
            let (conn, _) = listener.accept().await.unwrap();
            nuclei::spawn(n_echo(conn)).detach();
        }
    })
    .detach();
    rx.await.unwrap()
}

async fn n_asyncrw_client(addr: std::net::SocketAddr, rounds: u64) {
    use nuclei::io::{AsyncReadExt, AsyncWriteExt};
    let mut conn = NTcp::connect(addr).await.unwrap();
    let request = [7u8; 64];
    let mut reply = [0u8; 64];
    for _ in 0..rounds {
        conn.write_all(&request).await.unwrap();
        conn.read_exact(&mut reply).await.unwrap();
    }
}

async fn n_asyncrw_echo(listener: nuclei::net::TcpListener) {
    use nuclei::io::{AsyncReadExt, AsyncWriteExt};
    let (mut conn, _) = listener.accept().await.unwrap();
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = conn.read(&mut buf).await {
        conn.write_all(&buf[..n]).await.unwrap();
    }
}

async fn n_split_client(addr: std::net::SocketAddr, rounds: u64) {
    use nuclei::io::{AsyncReadExt, AsyncWriteExt};
    let (mut read, mut write) = NTcp::connect(addr).await.unwrap().into_split();
    let writer = nuclei::spawn(async move {
        let request = [7u8; 64];
        for _ in 0..rounds {
            write.write_all(&request).await.unwrap();
        }
    });
    let reader = nuclei::spawn(async move {
        let mut reply = [0u8; 64];
        for _ in 0..rounds {
            read.read_exact(&mut reply).await.unwrap();
        }
    });
    writer.await;
    reader.await;
}

#[cfg(unix)]
fn dgram_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let a = dir.join(format!("nuclei-op-count-{pid}-a"));
    let b = dir.join(format!("nuclei-op-count-{pid}-b"));
    let _ = (std::fs::remove_file(&a), std::fs::remove_file(&b));
    (a, b)
}

/// The backend the runtime's shards drive their IO with, as the runtime reports it.
pub fn backend(rt: &nuclei::runtime::Runtime) -> nuclei::IoBackend {
    rt.block_on(async { nuclei::Proactor::backend() })
}

/// The runtime a row runs on. `tcp_watched` keeps the handoff watch, with a threshold no poll
/// of the row reaches: its workers pay the watch's every store and look, and the monitor, whose
/// ticks go by time and not by operation, ticks once in a quarter of an hour.
pub fn runtime(row: &str) -> nuclei::runtime::Runtime {
    nuclei::runtime::Builder::new()
        .worker_threads(if row == "tcp_2w" { 2 } else { 1 })
        .pin_workers(false)
        .handoff_after((row == "tcp_watched").then_some(Duration::from_secs(3600)))
        .build()
        .expect("the runtime starts")
}

/// Runs `n` iterations of `row` on `rt` and returns the operations counted, or `None` for a
/// row this platform does not have. An iteration is a round trip, a spawn, a timeout, a
/// chunk; for `tcp_many` and `sleep` it is a round or a sleep on each of their tasks.
pub fn run(rt: &nuclei::runtime::Runtime, row: &str, n: u64) -> Option<u64> {
    let ops = match row {
        "block_on" => {
            for _ in 0..n {
                std::hint::black_box(rt.block_on(async { 1u8 }));
            }
            n
        }
        "mpsc" => {
            let (to_b, mut from_a) = nuclei::sync::mpsc::channel::<u64>(1);
            let (to_a, mut from_b) = nuclei::sync::mpsc::channel::<u64>(1);
            let a = rt.spawn(async move {
                for i in 0..n {
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
            n * 2
        }
        "timeout" => {
            rt.block_on(rt.spawn(async move {
                for _ in 0..n {
                    nuclei::time::timeout(Duration::from_secs(1), async {})
                        .await
                        .unwrap();
                }
            }));
            n
        }
        "tcp" | "tcp_2w" | "tcp_watched" => {
            let (tx, rx) = std::sync::mpsc::channel();
            rt.spawn(async move {
                let listener = nuclei::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                let (conn, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = conn.recv(&mut buf).await {
                    let mut at = 0;
                    while at < n {
                        at += conn.send(&buf[at..n]).await.unwrap();
                    }
                }
            })
            .detach();
            let addr = rx.recv().unwrap();
            rt.block_on(rt.spawn(async move {
                let conn = nuclei::net::TcpStream::connect(addr).await.unwrap();
                let request = [7u8; 64];
                let mut reply = [0u8; 64];
                for _ in 0..n {
                    let mut at = 0;
                    while at < 64 {
                        at += conn.send(&request[at..]).await.unwrap();
                    }
                    at = 0;
                    while at < 64 {
                        at += conn.recv(&mut reply[at..]).await.unwrap();
                    }
                }
            }));
            n
        }
        #[cfg(unix)]
        "unix_dgram" => {
            let (a_path, b_path) = dgram_paths();
            let (a_to, b_to) = (a_path.clone(), b_path.clone());
            rt.block_on(rt.spawn(async move {
                let a = nuclei::net::UnixDatagram::bind(&a_path).unwrap();
                let echo = nuclei::net::UnixDatagram::bind(&b_path).unwrap();
                nuclei::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok((n @ 1.., _)) = echo.recv_from(&mut buf).await {
                        echo.send_to(&buf[..n], &a_to).await.unwrap();
                    }
                })
                .detach();
                let request = [7u8; 64];
                let mut reply = [0u8; 64];
                for _ in 0..n {
                    a.send_to(&request, &b_to).await.unwrap();
                    a.recv_from(&mut reply).await.unwrap();
                }
                let _ = (std::fs::remove_file(&a_path), std::fs::remove_file(&b_path));
            }));
            n
        }
        "udp" => {
            rt.block_on(rt.spawn(async move {
                let a = nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let echo = nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let (a_addr, echo_addr) = (a.local_addr().unwrap(), echo.local_addr().unwrap());
                let server = nuclei::spawn(async move {
                    let mut buf = [0u8; 256];
                    for _ in 0..n {
                        let (len, _) = echo.recv_from(&mut buf).await.unwrap();
                        echo.send_to(&buf[..len], a_addr).await.unwrap();
                    }
                });
                let request = [3u8; 64];
                let mut reply = [0u8; 256];
                for _ in 0..n {
                    a.send_to(&request, echo_addr).await.unwrap();
                    a.recv_from(&mut reply).await.unwrap();
                }
                server.await;
            }));
            n
        }
        #[cfg(unix)]
        "unix_stream" => {
            let path = tmp_path("us");
            let _ = std::fs::remove_file(&path);
            let at = path.clone();
            rt.block_on(rt.spawn(async move {
                let listener = nuclei::net::UnixListener::bind(&at).unwrap();
                nuclei::spawn(async move {
                    let (conn, _) = listener.accept().await.unwrap();
                    let mut buf = [0u8; 4096];
                    while let Ok(len @ 1..) = conn.recv(&mut buf).await {
                        let mut sent = 0;
                        while sent < len {
                            sent += conn.send(&buf[sent..len]).await.unwrap();
                        }
                    }
                })
                .detach();
                let conn = nuclei::net::UnixStream::connect(&at).await.unwrap();
                let request = [7u8; 64];
                let mut reply = [0u8; 64];
                for _ in 0..n {
                    let mut done = 0;
                    while done < 64 {
                        done += conn.send(&request[done..]).await.unwrap();
                    }
                    done = 0;
                    while done < 64 {
                        done += conn.recv(&mut reply[done..]).await.unwrap();
                    }
                }
            }));
            let _ = std::fs::remove_file(&path);
            n
        }
        "tcp_split" => {
            rt.block_on(rt.spawn(async move {
                let addr = n_listen_echo(1).await;
                n_split_client(addr, n).await;
            }));
            n
        }
        "tcp_asyncrw" => {
            rt.block_on(rt.spawn(async move {
                let listener = nuclei::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                nuclei::spawn(n_asyncrw_echo(listener)).detach();
                n_asyncrw_client(addr, n).await;
            }));
            n
        }
        "tcp_many" => {
            rt.block_on(rt.spawn(async move {
                let addr = n_listen_echo(MANY_CONNS).await;
                let tasks: Vec<_> = (0..MANY_CONNS)
                    .map(|_| {
                        nuclei::spawn(async move {
                            let conn = NTcp::connect(addr).await.unwrap();
                            n_rounds(&conn, n).await;
                        })
                    })
                    .collect();
                for task in tasks {
                    task.await;
                }
            }));
            MANY_CONNS * n
        }
        "bulk" => {
            let bulk = n as usize * BULK_CHUNK;
            let sink = rt.spawn(async move {
                // Buffers set on both ends, which stops the kernel's autotuning of them (the
                // size is held to the system's maximum): autotuned, they grow by the round trips
                // the kernel measures, so whether a chunk goes straight to the kernel or into the
                // write-behind (a copy of 64 KiB) followed the timing of the run, and the row
                // came out in one of two modes.
                let listening = nuclei::net::TcpSocket::new_v4().unwrap();
                listening.set_send_buffer_size(BULK_BUFFER).unwrap();
                listening.set_recv_buffer_size(BULK_BUFFER).unwrap();
                listening.bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let listener = listening.listen(16).unwrap();
                let addr = listener.local_addr().unwrap();
                let receiver = nuclei::spawn(async move {
                    let (conn, _) = listener.accept().await.unwrap();
                    let mut buf = vec![0u8; BULK_CHUNK];
                    let mut total = 0;
                    while total < bulk {
                        total += conn.recv(&mut buf).await.unwrap();
                    }
                    total
                });
                let connecting = nuclei::net::TcpSocket::new_v4().unwrap();
                connecting.set_send_buffer_size(BULK_BUFFER).unwrap();
                connecting.set_recv_buffer_size(BULK_BUFFER).unwrap();
                let conn = connecting.connect(addr).await.unwrap();
                let chunk = vec![1u8; BULK_CHUNK];
                let mut sent = 0;
                while sent < bulk {
                    sent += conn
                        .send(&chunk[..BULK_CHUNK.min(bulk - sent)])
                        .await
                        .unwrap();
                }
                receiver.await
            });
            assert_eq!(rt.block_on(sink), bulk);
            n
        }
        "file_read" => {
            let size = n as usize * FILE_CHUNK;
            let path = bench_file(size);
            let total = rt.block_on(rt.spawn(async move {
                let file = nuclei::fs::File::open(&path).await.unwrap();
                let mut buf = Vec::with_capacity(FILE_CHUNK);
                let mut total = 0;
                loop {
                    buf.clear();
                    let (read, back) = file.read_owned(buf).await;
                    buf = back;
                    match read.unwrap() {
                        0 => break total,
                        len => total += len,
                    }
                }
            }));
            assert_eq!(total, size);
            n
        }
        "spawn_join" => {
            rt.block_on(rt.spawn(async move {
                for i in 0..n {
                    std::hint::black_box(nuclei::spawn(async move { i }).await);
                }
            }));
            n
        }
        "spawn_blocking" => {
            rt.block_on(rt.spawn(async move {
                for i in 0..n {
                    std::hint::black_box(nuclei::runtime::spawn_blocking(move || i).await);
                }
            }));
            n
        }
        "sleep" => {
            rt.block_on(rt.spawn(async move {
                let tasks: Vec<_> = (0..SLEEP_TASKS)
                    .map(|_| {
                        nuclei::spawn(async move {
                            for _ in 0..n {
                                nuclei::time::sleep(Duration::from_millis(1)).await;
                            }
                        })
                    })
                    .collect();
                for task in tasks {
                    task.await;
                }
            }));
            SLEEP_TASKS * n
        }
        "oneshot" => {
            rt.block_on(rt.spawn(async move {
                for i in 0..n {
                    let (tx, rx) = nuclei::sync::oneshot::channel::<u64>();
                    nuclei::spawn(async move {
                        let _ = tx.send(i);
                    })
                    .detach();
                    std::hint::black_box(rx.await.unwrap());
                }
            }));
            n
        }
        "local_mpsc" => {
            rt.block_on(rt.spawn(async move {
                let (to_b, mut from_a) = nuclei::sync::local::mpsc::channel::<u64>(1);
                let (to_a, mut from_b) = nuclei::sync::local::mpsc::channel::<u64>(1);
                let a = nuclei::task::spawn_local(async move {
                    for i in 0..n {
                        to_b.send(i).await.unwrap();
                        from_b.recv().await.unwrap();
                    }
                });
                let echo = nuclei::task::spawn_local(async move {
                    while let Some(i) = from_a.recv().await {
                        if to_a.send(i).await.is_err() {
                            break;
                        }
                    }
                });
                a.await;
                echo.await;
            }));
            n * 2
        }
        "local_oneshot" => {
            rt.block_on(rt.spawn(async move {
                nuclei::task::spawn_local(async move {
                    for i in 0..n {
                        let (tx, rx) = nuclei::sync::local::oneshot::channel::<u64>();
                        nuclei::task::spawn_local(async move {
                            let _ = tx.send(i);
                        })
                        .detach();
                        std::hint::black_box(rx.await.unwrap());
                    }
                })
                .await;
            }));
            n
        }
        _ => return None,
    };
    Some(ops)
}
