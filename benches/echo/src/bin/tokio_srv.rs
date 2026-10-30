use echo::{Args, announce, pin, ready, reuseport_listener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn serve(listener: TcpListener, buf_len: usize) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(echo(stream, buf_len));
    }
}

async fn echo(mut stream: TcpStream, buf_len: usize) {
    let mut buf = vec![0u8; buf_len];
    while let Ok(n @ 1..) = stream.read(&mut buf).await {
        if stream.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
}

fn main() {
    let a = Args::parse();
    let buf_len = a.buf_len();
    let barrier = ready(if a.mode == "ct" { a.cores } else { 0 });
    if a.mode == "ct" {
        let handles: Vec<_> = (0..a.cores)
            .map(|i| {
                let (port, cpu, b) = (a.port, a.first_cpu + i, barrier.clone());
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
                        serve(l, buf_len).await;
                    });
                })
            })
            .collect();
        announce(&barrier);
        for h in handles {
            let _ = h.join();
        }
    } else {
        let cpus: Vec<usize> = (a.first_cpu..a.first_cpu + a.cores).collect();
        pin(&cpus);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(a.cores)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let l = TcpListener::from_std(reuseport_listener(a.port, true).unwrap()).unwrap();
            announce(&barrier);
            serve(l, buf_len).await;
        });
    }
}
