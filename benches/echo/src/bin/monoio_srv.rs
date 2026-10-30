use echo::{Args, announce, pin, ready, reuseport_listener};
use monoio::buf::IoBuf;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};

async fn echo(mut stream: TcpStream, buf_len: usize) {
    let mut buf = vec![0u8; buf_len];
    loop {
        let (res, b) = stream.read(buf).await;
        buf = b;
        let Ok(n @ 1..) = res else { break };
        let (res, b) = stream.write_all(buf.slice(..n)).await;
        buf = b.into_inner();
        if res.is_err() {
            break;
        }
    }
}

async fn serve(port: u16, buf_len: usize) {
    let l = TcpListener::from_std(reuseport_listener(port, false).unwrap()).unwrap();
    loop {
        let Ok((stream, _)) = l.accept().await else {
            continue;
        };
        monoio::spawn(echo(stream, buf_len));
    }
}

fn main() {
    let a = Args::parse();
    let buf_len = a.buf_len();
    let barrier = ready(a.cores);
    let handles: Vec<_> = (0..a.cores)
        .map(|i| {
            let (port, cpu, mode, b) = (a.port, a.first_cpu + i, a.mode.clone(), barrier.clone());
            std::thread::spawn(move || {
                pin(&[cpu]);
                if mode == "legacy" {
                    let mut rt = monoio::RuntimeBuilder::<monoio::LegacyDriver>::new()
                        .enable_all()
                        .build()
                        .unwrap();
                    b.wait();
                    rt.block_on(serve(port, buf_len));
                } else {
                    let mut rt = monoio::RuntimeBuilder::<monoio::IoUringDriver>::new()
                        .with_entries(4096)
                        .enable_all()
                        .build()
                        .unwrap();
                    b.wait();
                    rt.block_on(serve(port, buf_len));
                }
            })
        })
        .collect();
    announce(&barrier);
    for h in handles {
        let _ = h.join();
    }
}
