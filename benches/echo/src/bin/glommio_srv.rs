use echo::{Args, announce, raise_nofile, ready, response, reuseport_listener};
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::net::{TcpListener, TcpStream};
use glommio::{LocalExecutorBuilder, Placement};
use std::os::fd::{FromRawFd, IntoRawFd};
use std::rc::Rc;

async fn serve(mut stream: TcpStream, req: usize, buf_len: usize, out: Rc<Vec<u8>>) {
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
    let (req, resp, buf_len) = (a.req, a.resp, a.buf_len());
    let barrier = ready(a.cores());
    let handles: Vec<_> = a
        .cpus
        .iter()
        .map(|&cpu| {
            let (port, b) = (a.port, barrier.clone());
            LocalExecutorBuilder::new(Placement::Fixed(cpu))
                .spawn(move || async move {
                    let fd = reuseport_listener(port, false).unwrap().into_raw_fd();
                    // SAFETY: the descriptor is a bound, listening TCP socket this executor now owns.
                    let l = unsafe { TcpListener::from_raw_fd(fd) };
                    let out = Rc::new(response(resp));
                    b.wait();
                    loop {
                        let Ok(stream) = l.accept().await else {
                            continue;
                        };
                        glommio::spawn_local(serve(stream, req, buf_len, out.clone())).detach();
                    }
                })
                .unwrap()
        })
        .collect();
    announce(&barrier, nofile);
    for h in handles {
        let _ = h.join();
    }
}
