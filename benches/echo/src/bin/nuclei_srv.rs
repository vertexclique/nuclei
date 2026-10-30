use echo::{Args, announce, pin, ready, reuseport_listener};
use nuclei::net::TcpListener;
use nuclei::runtime::Builder;

fn main() {
    let a = Args::parse();
    let buf_len = a.buf_len();
    let barrier = ready(a.cores);
    let handles: Vec<_> = (0..a.cores)
        .map(|i| {
            let (port, cpu, b) = (a.port, a.first_cpu + i, barrier.clone());
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
                        nuclei::spawn(async move {
                            let mut buf = vec![0u8; buf_len];
                            while let Ok(n @ 1..) = stream.recv(&mut buf).await {
                                if stream.send(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        })
                        .detach();
                    }
                }));
            })
        })
        .collect();
    announce(&barrier);
    for h in handles {
        let _ = h.join();
    }
}
