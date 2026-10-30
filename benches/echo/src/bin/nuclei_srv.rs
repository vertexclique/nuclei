use echo::{Args, announce, pin, raise_nofile, ready, response, reuseport_listener};
use nuclei::net::{TcpListener, TcpStream};
use nuclei::runtime::Builder;
use std::io;
use std::sync::Arc;

async fn send_all(stream: &TcpStream, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = stream.send(buf).await?;
        buf = &buf[n..];
    }
    Ok(())
}

async fn serve(stream: TcpStream, req: usize, buf_len: usize, out: Arc<Vec<u8>>) {
    let mut buf = vec![0u8; buf_len];
    let mut pending = 0;
    while let Ok(n @ 1..) = stream.recv(&mut buf).await {
        pending += n;
        while pending >= req {
            pending -= req;
            if send_all(&stream, &out).await.is_err() {
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
    let barrier = ready(a.cores());
    let handles: Vec<_> = a
        .cpus
        .iter()
        .map(|&cpu| {
            let (port, b, out) = (a.port, barrier.clone(), out.clone());
            std::thread::spawn(move || {
                pin(&[cpu]);
                let rt = Builder::new()
                    .worker_threads(1)
                    .pin_workers(false)
                    .handoff_after(None)
                    .build()
                    .unwrap();
                rt.block_on(rt.spawn(async move {
                    let l = TcpListener::new(reuseport_listener(port, true).unwrap()).unwrap();
                    b.wait();
                    loop {
                        let Ok((stream, _)) = l.accept().await else {
                            continue;
                        };
                        nuclei::spawn(serve(stream, req, buf_len, out.clone())).detach();
                    }
                }));
            })
        })
        .collect();
    announce(&barrier, nofile);
    for h in handles {
        let _ = h.join();
    }
}
