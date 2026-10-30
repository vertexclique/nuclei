//! A fixed amount of work on one runtime, for counting instructions and cycles:
//! `perf stat -e instructions:u,cycles:u target/release/examples/vs_tokio_count
//! <nuclei|tokio> <mode>`. Prints the iteration count so the caller divides.
//! Shapes match `vs_tokio`: one worker, unpinned, no handoff on nuclei; a
//! current-thread runtime on tokio. `tcp_2w` uses two workers on nuclei and a
//! two-worker multi-thread runtime on tokio.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BLOCK_ON: u64 = 200_000;
const MPSC_ROUNDS: u64 = 200_000;
const TIMEOUTS: u64 = 200_000;
const TCP_ROUNDS: u64 = 50_000;
#[cfg(unix)]
const DGRAM_ROUNDS: u64 = 50_000;
const UDP_ROUNDS: u64 = 50_000;
#[cfg(unix)]
const UNIX_ROUNDS: u64 = 50_000;
const MANY_CONNS: u64 = 256;
const MANY_ROUNDS: u64 = 20;
const BULK: usize = 64 << 20;
const BULK_CHUNK: usize = 64 << 10;
const FILE_SIZE: usize = 16 << 20;
const FILE_CHUNK: usize = 64 << 10;
const SPAWNS: u64 = 100_000;
const BLOCKING: u64 = 20_000;
const SLEEP_TASKS: u64 = 1_000;
const SLEEPS_PER_TASK: u64 = 20;
const ONESHOTS: u64 = 100_000;
const MODES: &str = "block_on|mpsc|timeout|tcp|unix_dgram|udp|unix_stream|tcp_split|tcp_asyncrw|tcp_many|tcp_2w|bulk|file_read|spawn_join|spawn_blocking|sleep|oneshot";

#[cfg(unix)]
fn tmp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("vs-tokio-count-{}-{tag}", std::process::id()))
}

fn bench_file() -> std::path::PathBuf {
    let path = std::env::temp_dir().join("vs-tokio-count-file-16MiB");
    if std::fs::metadata(&path).map_or(true, |m| m.len() != FILE_SIZE as u64) {
        std::fs::write(&path, vec![5u8; FILE_SIZE]).expect("the file is written");
    }
    path
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

async fn t_echo(mut conn: tokio::net::TcpStream) {
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = conn.read(&mut buf).await {
        if conn.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

async fn t_rounds(conn: &mut tokio::net::TcpStream, rounds: u64) {
    let request = [7u8; 64];
    let mut reply = [0u8; 64];
    for _ in 0..rounds {
        conn.write_all(&request).await.unwrap();
        conn.read_exact(&mut reply).await.unwrap();
    }
}

async fn t_listen_echo(accepts: u64) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..accepts {
            let (conn, _) = listener.accept().await.unwrap();
            tokio::spawn(t_echo(conn));
        }
    });
    addr
}

async fn n_asyncrw_client(addr: std::net::SocketAddr) {
    use nuclei::io::{AsyncReadExt, AsyncWriteExt};
    let mut conn = NTcp::connect(addr).await.unwrap();
    let request = [7u8; 64];
    let mut reply = [0u8; 64];
    for _ in 0..TCP_ROUNDS {
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

async fn n_split_client(addr: std::net::SocketAddr) {
    use nuclei::io::{AsyncReadExt, AsyncWriteExt};
    let (mut read, mut write) = NTcp::connect(addr).await.unwrap().into_split();
    let writer = nuclei::spawn(async move {
        let request = [7u8; 64];
        for _ in 0..TCP_ROUNDS {
            write.write_all(&request).await.unwrap();
        }
    });
    let reader = nuclei::spawn(async move {
        let mut reply = [0u8; 64];
        for _ in 0..TCP_ROUNDS {
            read.read_exact(&mut reply).await.unwrap();
        }
    });
    writer.await;
    reader.await;
}

async fn t_split_client(addr: std::net::SocketAddr) {
    let (mut read, mut write) = tokio::net::TcpStream::connect(addr)
        .await
        .unwrap()
        .into_split();
    let writer = tokio::spawn(async move {
        let request = [7u8; 64];
        for _ in 0..TCP_ROUNDS {
            write.write_all(&request).await.unwrap();
        }
    });
    let reader = tokio::spawn(async move {
        let mut reply = [0u8; 64];
        for _ in 0..TCP_ROUNDS {
            read.read_exact(&mut reply).await.unwrap();
        }
    });
    writer.await.unwrap();
    reader.await.unwrap();
}

#[cfg(unix)]
fn dgram_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let a = dir.join(format!("vs-tokio-count-{pid}-a"));
    let b = dir.join(format!("vs-tokio-count-{pid}-b"));
    let _ = (std::fs::remove_file(&a), std::fs::remove_file(&b));
    (a, b)
}

fn nuclei_run(row: &str) -> u64 {
    let rt = nuclei::runtime::Builder::new()
        .worker_threads(if row == "tcp_2w" { 2 } else { 1 })
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .expect("the runtime starts");
    match row {
        "block_on" => {
            for _ in 0..BLOCK_ON {
                std::hint::black_box(rt.block_on(async { 1u8 }));
            }
            BLOCK_ON
        }
        "mpsc" => {
            let (to_b, mut from_a) = nuclei::sync::mpsc::channel::<u64>(1);
            let (to_a, mut from_b) = nuclei::sync::mpsc::channel::<u64>(1);
            let a = rt.spawn(async move {
                for i in 0..MPSC_ROUNDS {
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
            MPSC_ROUNDS * 2
        }
        "timeout" => {
            rt.block_on(rt.spawn(async {
                for _ in 0..TIMEOUTS {
                    nuclei::time::timeout(Duration::from_secs(1), async {})
                        .await
                        .unwrap();
                }
            }));
            TIMEOUTS
        }
        "tcp" | "tcp_2w" => {
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
                for _ in 0..TCP_ROUNDS {
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
            TCP_ROUNDS
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
                for _ in 0..DGRAM_ROUNDS {
                    a.send_to(&request, &b_to).await.unwrap();
                    a.recv_from(&mut reply).await.unwrap();
                }
                let _ = (std::fs::remove_file(&a_path), std::fs::remove_file(&b_path));
            }));
            DGRAM_ROUNDS
        }
        "udp" => {
            rt.block_on(rt.spawn(async {
                let a = nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let echo = nuclei::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let (a_addr, echo_addr) = (a.local_addr().unwrap(), echo.local_addr().unwrap());
                let server = nuclei::spawn(async move {
                    let mut buf = [0u8; 256];
                    for _ in 0..UDP_ROUNDS {
                        let (n, _) = echo.recv_from(&mut buf).await.unwrap();
                        echo.send_to(&buf[..n], a_addr).await.unwrap();
                    }
                });
                let request = [3u8; 64];
                let mut reply = [0u8; 256];
                for _ in 0..UDP_ROUNDS {
                    a.send_to(&request, echo_addr).await.unwrap();
                    a.recv_from(&mut reply).await.unwrap();
                }
                server.await;
            }));
            UDP_ROUNDS
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
                    while let Ok(n @ 1..) = conn.recv(&mut buf).await {
                        let mut sent = 0;
                        while sent < n {
                            sent += conn.send(&buf[sent..n]).await.unwrap();
                        }
                    }
                })
                .detach();
                let conn = nuclei::net::UnixStream::connect(&at).await.unwrap();
                let request = [7u8; 64];
                let mut reply = [0u8; 64];
                for _ in 0..UNIX_ROUNDS {
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
            UNIX_ROUNDS
        }
        "tcp_split" => {
            rt.block_on(rt.spawn(async {
                let addr = n_listen_echo(1).await;
                n_split_client(addr).await;
            }));
            TCP_ROUNDS
        }
        "tcp_asyncrw" => {
            rt.block_on(rt.spawn(async {
                let listener = nuclei::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                nuclei::spawn(n_asyncrw_echo(listener)).detach();
                n_asyncrw_client(addr).await;
            }));
            TCP_ROUNDS
        }
        "tcp_many" => {
            rt.block_on(rt.spawn(async {
                let addr = n_listen_echo(MANY_CONNS).await;
                let tasks: Vec<_> = (0..MANY_CONNS)
                    .map(|_| {
                        nuclei::spawn(async move {
                            let conn = NTcp::connect(addr).await.unwrap();
                            n_rounds(&conn, MANY_ROUNDS).await;
                        })
                    })
                    .collect();
                for task in tasks {
                    task.await;
                }
            }));
            MANY_CONNS * MANY_ROUNDS
        }
        "bulk" => {
            let sink = rt.spawn(async move {
                let listener = nuclei::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let receiver = nuclei::spawn(async move {
                    let (conn, _) = listener.accept().await.unwrap();
                    let mut buf = vec![0u8; BULK_CHUNK];
                    let mut total = 0;
                    while total < BULK {
                        total += conn.recv(&mut buf).await.unwrap();
                    }
                    total
                });
                let conn = NTcp::connect(addr).await.unwrap();
                let chunk = vec![1u8; BULK_CHUNK];
                let mut sent = 0;
                while sent < BULK {
                    sent += conn
                        .send(&chunk[..BULK_CHUNK.min(BULK - sent)])
                        .await
                        .unwrap();
                }
                receiver.await
            });
            assert_eq!(rt.block_on(sink), BULK);
            (BULK / BULK_CHUNK) as u64
        }
        "file_read" => {
            let path = bench_file();
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
                        n => total += n,
                    }
                }
            }));
            assert_eq!(total, FILE_SIZE);
            (FILE_SIZE / FILE_CHUNK) as u64
        }
        "spawn_join" => {
            rt.block_on(rt.spawn(async {
                for i in 0..SPAWNS {
                    std::hint::black_box(nuclei::spawn(async move { i }).await);
                }
            }));
            SPAWNS
        }
        "spawn_blocking" => {
            rt.block_on(rt.spawn(async {
                for i in 0..BLOCKING {
                    std::hint::black_box(nuclei::runtime::spawn_blocking(move || i).await);
                }
            }));
            BLOCKING
        }
        "sleep" => {
            rt.block_on(rt.spawn(async {
                let tasks: Vec<_> = (0..SLEEP_TASKS)
                    .map(|_| {
                        nuclei::spawn(async {
                            for _ in 0..SLEEPS_PER_TASK {
                                nuclei::time::sleep(Duration::from_millis(1)).await;
                            }
                        })
                    })
                    .collect();
                for task in tasks {
                    task.await;
                }
            }));
            SLEEP_TASKS * SLEEPS_PER_TASK
        }
        "oneshot" => {
            rt.block_on(rt.spawn(async {
                for i in 0..ONESHOTS {
                    let (tx, rx) = nuclei::sync::oneshot::channel::<u64>();
                    nuclei::spawn(async move {
                        let _ = tx.send(i);
                    })
                    .detach();
                    std::hint::black_box(rx.await.unwrap());
                }
            }));
            ONESHOTS
        }
        _ => usage(),
    }
}

fn tokio_run(row: &str) -> u64 {
    let mut builder = if row == "tcp_2w" {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    if row == "tcp_2w" {
        builder.worker_threads(2);
    }
    let rt = builder.enable_all().build().expect("the runtime starts");
    match row {
        "block_on" => {
            for _ in 0..BLOCK_ON {
                std::hint::black_box(rt.block_on(async { 1u8 }));
            }
            BLOCK_ON
        }
        "mpsc" => {
            let (to_b, mut from_a) = tokio::sync::mpsc::channel::<u64>(1);
            let (to_a, mut from_b) = tokio::sync::mpsc::channel::<u64>(1);
            let a = rt.spawn(async move {
                for i in 0..MPSC_ROUNDS {
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
            MPSC_ROUNDS * 2
        }
        "timeout" => {
            rt.block_on(rt.spawn(async {
                for _ in 0..TIMEOUTS {
                    tokio::time::timeout(Duration::from_secs(1), async {})
                        .await
                        .unwrap();
                }
            }))
            .unwrap();
            TIMEOUTS
        }
        "tcp" | "tcp_2w" => {
            let listener = rt
                .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .unwrap();
            let addr = listener.local_addr().unwrap();
            rt.spawn(async move {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = conn.read(&mut buf).await {
                    conn.write_all(&buf[..n]).await.unwrap();
                }
            });
            rt.block_on(rt.spawn(async move {
                let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
                let request = [7u8; 64];
                let mut reply = [0u8; 64];
                for _ in 0..TCP_ROUNDS {
                    conn.write_all(&request).await.unwrap();
                    conn.read_exact(&mut reply).await.unwrap();
                }
            }))
            .unwrap();
            TCP_ROUNDS
        }
        #[cfg(unix)]
        "unix_dgram" => {
            let (a_path, b_path) = dgram_paths();
            let (a_to, b_to) = (a_path.clone(), b_path.clone());
            rt.block_on(rt.spawn(async move {
                let a = tokio::net::UnixDatagram::bind(&a_path).unwrap();
                let echo = tokio::net::UnixDatagram::bind(&b_path).unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok((n @ 1.., _)) = echo.recv_from(&mut buf).await {
                        echo.send_to(&buf[..n], &a_to).await.unwrap();
                    }
                });
                let request = [7u8; 64];
                let mut reply = [0u8; 64];
                for _ in 0..DGRAM_ROUNDS {
                    a.send_to(&request, &b_to).await.unwrap();
                    a.recv_from(&mut reply).await.unwrap();
                }
                let _ = (std::fs::remove_file(&a_path), std::fs::remove_file(&b_path));
            }))
            .unwrap();
            DGRAM_ROUNDS
        }
        "udp" => {
            rt.block_on(rt.spawn(async {
                let a = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let echo = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let (a_addr, echo_addr) = (a.local_addr().unwrap(), echo.local_addr().unwrap());
                let server = tokio::spawn(async move {
                    let mut buf = [0u8; 256];
                    for _ in 0..UDP_ROUNDS {
                        let (n, _) = echo.recv_from(&mut buf).await.unwrap();
                        echo.send_to(&buf[..n], a_addr).await.unwrap();
                    }
                });
                let request = [3u8; 64];
                let mut reply = [0u8; 256];
                for _ in 0..UDP_ROUNDS {
                    a.send_to(&request, echo_addr).await.unwrap();
                    a.recv_from(&mut reply).await.unwrap();
                }
                server.await.unwrap();
            }))
            .unwrap();
            UDP_ROUNDS
        }
        #[cfg(unix)]
        "unix_stream" => {
            let path = tmp_path("us");
            let _ = std::fs::remove_file(&path);
            let at = path.clone();
            rt.block_on(rt.spawn(async move {
                let listener = tokio::net::UnixListener::bind(&at).unwrap();
                tokio::spawn(async move {
                    let (mut conn, _) = listener.accept().await.unwrap();
                    let mut buf = [0u8; 4096];
                    while let Ok(n @ 1..) = conn.read(&mut buf).await {
                        conn.write_all(&buf[..n]).await.unwrap();
                    }
                });
                let mut conn = tokio::net::UnixStream::connect(&at).await.unwrap();
                let request = [7u8; 64];
                let mut reply = [0u8; 64];
                for _ in 0..UNIX_ROUNDS {
                    conn.write_all(&request).await.unwrap();
                    conn.read_exact(&mut reply).await.unwrap();
                }
            }))
            .unwrap();
            let _ = std::fs::remove_file(&path);
            UNIX_ROUNDS
        }
        "tcp_split" => {
            rt.block_on(rt.spawn(async {
                let addr = t_listen_echo(1).await;
                t_split_client(addr).await;
            }))
            .unwrap();
            TCP_ROUNDS
        }
        "tcp_asyncrw" => {
            rt.block_on(rt.spawn(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move {
                    let (mut conn, _) = listener.accept().await.unwrap();
                    let mut buf = [0u8; 4096];
                    while let Ok(n @ 1..) = conn.read(&mut buf).await {
                        conn.write_all(&buf[..n]).await.unwrap();
                    }
                });
                let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
                t_rounds(&mut conn, TCP_ROUNDS).await;
            }))
            .unwrap();
            TCP_ROUNDS
        }
        "tcp_many" => {
            rt.block_on(rt.spawn(async {
                let addr = t_listen_echo(MANY_CONNS).await;
                let tasks: Vec<_> = (0..MANY_CONNS)
                    .map(|_| {
                        tokio::spawn(async move {
                            let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
                            t_rounds(&mut conn, MANY_ROUNDS).await;
                        })
                    })
                    .collect();
                for task in tasks {
                    task.await.unwrap();
                }
            }))
            .unwrap();
            MANY_CONNS * MANY_ROUNDS
        }
        "bulk" => {
            let sink = rt.spawn(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let receiver = tokio::spawn(async move {
                    let (mut conn, _) = listener.accept().await.unwrap();
                    let mut buf = vec![0u8; BULK_CHUNK];
                    let mut total = 0;
                    while total < BULK {
                        total += conn.read(&mut buf).await.unwrap();
                    }
                    total
                });
                let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
                let chunk = vec![1u8; BULK_CHUNK];
                let mut sent = 0;
                while sent < BULK {
                    let n = BULK_CHUNK.min(BULK - sent);
                    conn.write_all(&chunk[..n]).await.unwrap();
                    sent += n;
                }
                receiver.await.unwrap()
            });
            assert_eq!(rt.block_on(sink).unwrap(), BULK);
            (BULK / BULK_CHUNK) as u64
        }
        "file_read" => {
            let path = bench_file();
            let total = rt
                .block_on(rt.spawn(async move {
                    let mut file = tokio::fs::File::open(&path).await.unwrap();
                    let mut buf = vec![0u8; FILE_CHUNK];
                    let mut total = 0;
                    loop {
                        match file.read(&mut buf).await.unwrap() {
                            0 => break total,
                            n => total += n,
                        }
                    }
                }))
                .unwrap();
            assert_eq!(total, FILE_SIZE);
            (FILE_SIZE / FILE_CHUNK) as u64
        }
        "spawn_join" => {
            rt.block_on(rt.spawn(async {
                for i in 0..SPAWNS {
                    std::hint::black_box(tokio::spawn(async move { i }).await.unwrap());
                }
            }))
            .unwrap();
            SPAWNS
        }
        "spawn_blocking" => {
            rt.block_on(rt.spawn(async {
                for i in 0..BLOCKING {
                    std::hint::black_box(tokio::task::spawn_blocking(move || i).await.unwrap());
                }
            }))
            .unwrap();
            BLOCKING
        }
        "sleep" => {
            rt.block_on(rt.spawn(async {
                let tasks: Vec<_> = (0..SLEEP_TASKS)
                    .map(|_| {
                        tokio::spawn(async {
                            for _ in 0..SLEEPS_PER_TASK {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                            }
                        })
                    })
                    .collect();
                for task in tasks {
                    task.await.unwrap();
                }
            }))
            .unwrap();
            SLEEP_TASKS * SLEEPS_PER_TASK
        }
        "oneshot" => {
            rt.block_on(rt.spawn(async {
                for i in 0..ONESHOTS {
                    let (tx, rx) = tokio::sync::oneshot::channel::<u64>();
                    tokio::spawn(async move {
                        let _ = tx.send(i);
                    });
                    std::hint::black_box(rx.await.unwrap());
                }
            }))
            .unwrap();
            ONESHOTS
        }
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: vs_tokio_count <nuclei|tokio> <{MODES}>");
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(side), Some(row)) = (args.next(), args.next()) else {
        usage()
    };
    let iterations = match side.as_str() {
        "nuclei" => nuclei_run(&row),
        "tokio" => tokio_run(&row),
        _ => usage(),
    };
    println!("{side} {row} iterations={iterations}");
}
