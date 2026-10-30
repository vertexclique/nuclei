use echo::{Args, announce, pin, raise_nofile, ready, response, reuseport_listener};
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};
use std::rc::Rc;

async fn serve(mut stream: TcpStream, req: usize, buf_len: usize, out: Rc<Vec<u8>>) {
    let mut buf = vec![0u8; buf_len];
    let mut pending = 0;
    loop {
        let (res, b) = stream.read(buf).await;
        buf = b;
        let Ok(n @ 1..) = res else { break };
        pending += n;
        while pending >= req {
            pending -= req;
            if stream.write_all(out.clone()).await.0.is_err() {
                return;
            }
        }
    }
}

async fn accept(l: std::net::TcpListener, req: usize, buf_len: usize, resp: usize) {
    let l = TcpListener::from_std(l).unwrap();
    let out = Rc::new(response(resp));
    loop {
        let Ok((stream, _)) = l.accept().await else {
            continue;
        };
        monoio::spawn(serve(stream, req, buf_len, out.clone()));
    }
}

fn main() {
    let nofile = raise_nofile();
    let a = Args::parse();
    let (req, resp, buf_len) = (a.req, a.resp, a.buf_len());
    let barrier = ready(a.cores());
    let handles: Vec<_> = a
        .cpus
        .iter()
        .map(|&cpu| {
            let (port, mode, b) = (a.port, a.mode.clone(), barrier.clone());
            std::thread::spawn(move || {
                pin(&[cpu]);
                if mode == "legacy" {
                    let mut rt = monoio::RuntimeBuilder::<monoio::LegacyDriver>::new()
                        .enable_all()
                        .build()
                        .unwrap();
                    let l = reuseport_listener(port, true).unwrap();
                    b.wait();
                    rt.block_on(accept(l, req, buf_len, resp));
                } else {
                    let mut rt = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
                        .with_entries(4096)
                        .enable_all()
                        .build()
                        .unwrap();
                    let l = reuseport_listener(port, false).unwrap();
                    b.wait();
                    rt.block_on(accept(l, req, buf_len, resp));
                }
            })
        })
        .collect();
    announce(&barrier, nofile);
    for h in handles {
        let _ = h.join();
    }
}
