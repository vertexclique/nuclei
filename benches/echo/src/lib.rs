use std::io;
use std::net::{SocketAddr, TcpListener};

/// The byte every response is made of; the client checks it.
pub const RESPONSE_BYTE: u8 = 0x5a;

/// A server's arguments: `<port> <cpu,cpu,...> <request bytes> <response bytes> [mode]`.
/// Each worker is pinned to one CPU of the list. A connection's every `req` bytes
/// are one request, answered with `resp` bytes of [`RESPONSE_BYTE`].
pub struct Args {
    pub port: u16,
    pub cpus: Vec<usize>,
    pub req: usize,
    pub resp: usize,
    pub mode: String,
}

impl Args {
    pub fn parse() -> Args {
        let a: Vec<String> = std::env::args().collect();
        let num = |i: usize, d: usize| a.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
        let cpus: Vec<usize> = a
            .get(2)
            .map(|s| s.split(',').filter_map(|c| c.parse().ok()).collect())
            .unwrap_or_default();
        let req = num(3, 64).max(1);
        Args {
            port: num(1, 7100) as u16,
            cpus: if cpus.is_empty() { vec![0] } else { cpus },
            req,
            resp: num(4, req).max(1),
            mode: a.get(5).cloned().unwrap_or_default(),
        }
    }

    pub fn cores(&self) -> usize {
        self.cpus.len()
    }

    pub fn buf_len(&self) -> usize {
        self.req.max(4096)
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

/// The CPUs the calling thread may run on, in order.
pub fn allowed_cpus() -> Vec<usize> {
    // SAFETY: a plain syscall into a zeroed set of our own.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return Vec::new();
        }
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&c| libc::CPU_ISSET(c, &set))
            .collect()
    }
}

/// Raises the soft RLIMIT_NOFILE to the hard limit; returns (before, after).
pub fn raise_nofile() -> (u64, u64) {
    // SAFETY: a plain out-parameter of our own, then the same struct back.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return (0, 0);
        }
        let before = lim.rlim_cur;
        lim.rlim_cur = lim.rlim_max;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
            return (before, before);
        }
        (before, lim.rlim_cur)
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

pub fn response(len: usize) -> Vec<u8> {
    vec![RESPONSE_BYTE; len]
}

pub fn ready(threads: usize) -> std::sync::Arc<std::sync::Barrier> {
    std::sync::Arc::new(std::sync::Barrier::new(threads + 1))
}

/// Waits for every worker's listener, then prints the line the runner waits for,
/// with the descriptor limit `raise_nofile` left.
pub fn announce(barrier: &std::sync::Barrier, nofile: (u64, u64)) {
    barrier.wait();
    println!("ready nofile={} nofile_before={}", nofile.1, nofile.0);
}
