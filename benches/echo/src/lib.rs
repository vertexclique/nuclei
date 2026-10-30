use std::io;
use std::net::{SocketAddr, TcpListener};

pub struct Args {
    pub port: u16,
    pub cores: usize,
    pub first_cpu: usize,
    pub msg: usize,
    pub mode: String,
}

impl Args {
    pub fn parse() -> Args {
        let a: Vec<String> = std::env::args().collect();
        let num = |i: usize, d: usize| a.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
        Args {
            port: num(1, 7100) as u16,
            cores: num(2, 1),
            first_cpu: num(3, 0),
            msg: num(4, 100),
            mode: a.get(5).cloned().unwrap_or_default(),
        }
    }

    pub fn buf_len(&self) -> usize {
        self.msg.max(4096)
    }
}

pub fn pin(cpus: &[usize]) {
    // SAFETY: cpu_set_t is plain data and sched_setaffinity(0, ..) reads it for the calling thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for &c in cpus {
            libc::CPU_SET(c, &mut set);
        }
        if libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) != 0 {
            eprintln!("pin failed: {}", io::Error::last_os_error());
            std::process::exit(2);
        }
    }
}

pub fn reuseport_listener(port: u16, nonblocking: bool) -> io::Result<TcpListener> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)?;
    sock.set_reuse_address(true)?;
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(nonblocking)?;
    sock.bind(&addr.into())?;
    sock.listen(4096)?;
    Ok(sock.into())
}

pub fn ready(threads: usize) -> std::sync::Arc<std::sync::Barrier> {
    std::sync::Arc::new(std::sync::Barrier::new(threads + 1))
}

pub fn announce(barrier: &std::sync::Barrier) {
    barrier.wait();
    println!("ready");
}
