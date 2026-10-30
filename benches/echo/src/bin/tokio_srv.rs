use echo::{Args, announce, pin, raise_nofile, ready, response, reuseport_listener};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn accept(listener: TcpListener, req: usize, buf_len: usize, out: Arc<Vec<u8>>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(serve(stream, req, buf_len, out.clone()));
    }
}

async fn serve(mut stream: TcpStream, req: usize, buf_len: usize, out: Arc<Vec<u8>>) {
    let mut buf = vec![0u8; buf_len];
    let mut pending = 0;
    while let Ok(n @ 1..) = stream.read(&mut buf).await {
        pending += n;
        while pending >= req {
            pending -= req;
            if stream.write_all(&out).await.is_err() {
                return;
            }
        }
    }
}

fn main() {
    let nofile = raise_nofile();
    let a = Args::parse();
    let (req, buf_len) = (a.req, a.buf_len());
    let out = Arc::new(response(a.resp));
    let barrier = ready(if a.mode == "ct" { a.cores() } else { 0 });
    if a.mode == "ct" {
        let handles: Vec<_> = a
            .cpus
            .iter()
            .map(|&cpu| {
                let (port, b, out) = (a.port, barrier.clone(), out.clone());
                std::thread::spawn(move || {
                    pin(&[cpu]);
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    rt.block_on(async {
                        let l =
                            TcpListener::from_std(reuseport_listener(port, true).unwrap()).unwrap();
                        b.wait();
                        accept(l, req, buf_len, out).await;
                    });
                })
            })
            .collect();
        announce(&barrier, nofile);
        for h in handles {
            let _ = h.join();
        }
    } else {
        pin(&a.cpus);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(a.cores())
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let l = TcpListener::from_std(reuseport_listener(a.port, true).unwrap()).unwrap();
            announce(&barrier, nofile);
            accept(l, req, buf_len, out).await;
        });
    }
}
