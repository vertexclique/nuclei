use echo::{Args, announce, ready};
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::net::{TcpListener, TcpStream};
use glommio::{LocalExecutorBuilder, Placement};

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
    let barrier = ready(a.cores);
    let handles: Vec<_> = (0..a.cores)
        .map(|i| {
            let (port, cpu, b) = (a.port, a.first_cpu + i, barrier.clone());
            LocalExecutorBuilder::new(Placement::Fixed(cpu))
                .spawn(move || async move {
                    let l = TcpListener::bind(("127.0.0.1", port)).unwrap();
                    b.wait();
                    loop {
                        let Ok(stream) = l.accept().await else {
                            continue;
                        };
                        glommio::spawn_local(echo(stream, buf_len)).detach();
                    }
                })
                .unwrap()
        })
        .collect();
    announce(&barrier);
    for h in handles {
        let _ = h.join();
    }
}
