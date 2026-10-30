//! A fixed amount of work on one runtime, for counting instructions and cycles:
//! `perf stat -e instructions:u,cycles:u target/release/examples/vs_tokio_count
//! <nuclei|tokio> <block_on|mpsc|timeout|tcp|unix_dgram>`. Prints the iteration count so
//! the caller divides. Shapes match `vs_tokio`: one worker, unpinned, no handoff
//! on nuclei; a current-thread runtime on tokio.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BLOCK_ON: u64 = 200_000;
const MPSC_ROUNDS: u64 = 200_000;
const TIMEOUTS: u64 = 200_000;
const TCP_ROUNDS: u64 = 50_000;
#[cfg(unix)]
const DGRAM_ROUNDS: u64 = 50_000;

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
        .worker_threads(1)
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
        "tcp" => {
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
        _ => usage(),
    }
}

fn tokio_run(row: &str) -> u64 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
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
        "tcp" => {
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
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: vs_tokio_count <nuclei|tokio> <block_on|mpsc|timeout|tcp|unix_dgram>");
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
