//! The three read paths behind one [`Reader`].

use super::*;

/// Submits and waits for one completion; every attempt counts as an `io_uring_enter`.
pub fn enter_wait(ring: &mut Ring, c: &mut Counters) -> io::Result<()> {
    loop {
        c.enter += 1;
        match ring.submit_and_wait(1, None) {
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            r => return r.map(drop),
        }
    }
}

/// Cancels the armed operation `ud` and reaps until its final completion and the cancel's
/// arrive, so the ring can drop with nothing in flight. Bounded: a lost completion is an error.
pub fn cancel_and_reap(ring: &mut Ring, ud: u64) -> io::Result<()> {
    ring.push(&Sqe::async_cancel(ud).user_data(CANCEL_UD))
        .map_err(io::Error::other)?;
    let (mut op_done, mut cancel_done) = (false, false);
    for _ in 0..100 {
        match ring.submit_and_wait(1, Some(Duration::from_millis(100))) {
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            r => {
                r?;
            }
        }
        for cqe in ring.completions() {
            match cqe.user_data {
                CANCEL_UD => cancel_done = true,
                u if u == ud && cqe.flags & IORING_CQE_F_MORE == 0 => op_done = true,
                _ => {}
            }
        }
        if op_done && cancel_done {
            return Ok(());
        }
    }
    Err(io::Error::other("the cancelled operation never completed"))
}

/// Path A: a provided-buffer pool behind a multishot `recv`.
pub struct PoolReader {
    // Field order is drop order: the pool unregisters before the ring goes away.
    pub pool: BufRing,
    pub ring: Ring,
    pub fd: RawFd,
    /// Completed pool buffers `(id, length)` not yet read.
    pub pending: VecDeque<(u16, u32)>,
    /// The buffer being consumed: `(id, length, offset)`.
    pub cur: Option<(u16, u32, u32)>,
    pub armed: bool,
    pub eof: bool,
    pub c: Counters,
}

/// How the pool for a read size is built.
#[derive(Clone, Copy, Debug)]
pub struct PoolCfg {
    pub buf_size: u32,
    pub count: u16,
}

impl PoolReader {
    pub fn new(fd: RawFd, cfg: PoolCfg) -> io::Result<Self> {
        let mut rc = RingConfig::new(256);
        // A completion queue deep enough that a burst of completions never ends the multishot.
        rc.cq_entries = Some(4096);
        let ring = Ring::new(&rc)?;
        let mut pool = BufRing::new(&ring, 0, cfg.count, cfg.buf_size)?;
        for bid in 0..cfg.count {
            pool.add(bid)?;
        }
        pool.commit();
        let mut r = Self {
            pool,
            ring,
            fd,
            pending: VecDeque::with_capacity(usize::from(cfg.count)),
            cur: None,
            armed: false,
            eof: false,
            c: Counters::default(),
        };
        r.arm()?;
        Ok(r)
    }

    pub fn arm(&mut self) -> io::Result<()> {
        let sqe = Sqe::recv_multishot(self.fd, self.pool.bgid(), 0).user_data(RECV_UD);
        self.ring.push(&sqe).map_err(io::Error::other)?;
        self.armed = true;
        Ok(())
    }

    /// Copies pooled data into `dst` until it is full or the pooled data is exhausted, recycling
    /// each buffer the moment it is empty.
    pub fn copy_out(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < dst.len() {
            let Some((bid, len, off)) = self
                .cur
                .take()
                .or_else(|| self.pending.pop_front().map(|(b, l)| (b, l, 0)))
            else {
                break;
            };
            let src = self
                .pool
                .buffer(bid, len)
                .ok_or_else(|| io::Error::other("a completion named a buffer outside the pool"))?;
            let n = ((len - off) as usize).min(dst.len() - filled);
            dst[filled..filled + n].copy_from_slice(&src[off as usize..off as usize + n]);
            filled += n;
            let off = off + n as u32;
            if off == len {
                self.pool.add(bid)?;
                self.pool.commit();
            } else {
                self.cur = Some((bid, len, off));
            }
        }
        Ok(filled)
    }

    /// Enters the kernel until at least one completion with data (or the end of stream) is in.
    pub fn wait(&mut self) -> io::Result<()> {
        loop {
            enter_wait(&mut self.ring, &mut self.c)?;
            for cqe in self.ring.completions() {
                if cqe.flags & IORING_CQE_F_MORE == 0 {
                    self.armed = false;
                }
                match cqe.res {
                    n if n > 0 => {
                        let bid = BufRing::buffer_id(cqe.flags)
                            .ok_or_else(|| io::Error::other("a receive used no pool buffer"))?;
                        self.pending.push_back((bid, n as u32));
                    }
                    0 => self.eof = true,
                    e if e == -libc::ENOBUFS => self.c.enobufs += 1,
                    e => return Err(io::Error::from_raw_os_error(-e)),
                }
            }
            if !self.armed && !self.eof {
                self.arm()?;
            }
            if self.eof || !self.pending.is_empty() {
                return Ok(());
            }
        }
    }

    pub fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        loop {
            let filled = self.copy_out(dst)?;
            if filled > 0 || self.eof {
                return Ok(filled);
            }
            self.wait()?;
        }
    }
}

impl Drop for PoolReader {
    fn drop(&mut self) {
        if self.armed
            && let Err(e) = cancel_and_reap(&mut self.ring, RECV_UD)
        {
            eprintln!("read_path: pool reader teardown failed: {e}");
        }
    }
}

/// How a direct reader learns the socket may have data.
pub trait Readiness {
    fn wait(&mut self, c: &mut Counters) -> io::Result<()>;
}

/// Path B: an io_uring multishot poll.
pub struct UringPoll {
    pub ring: Ring,
    pub fd: RawFd,
    pub armed: bool,
}

impl UringPoll {
    pub fn new(fd: RawFd) -> io::Result<Self> {
        let mut p = Self {
            ring: Ring::new(&RingConfig::new(64))?,
            fd,
            armed: false,
        };
        p.arm()?;
        Ok(p)
    }

    pub fn arm(&mut self) -> io::Result<()> {
        let sqe = Sqe::poll_add_multi(self.fd, libc::POLLIN as u32).user_data(RECV_UD);
        self.ring.push(&sqe).map_err(io::Error::other)?;
        self.armed = true;
        Ok(())
    }
}

impl Readiness for UringPoll {
    fn wait(&mut self, c: &mut Counters) -> io::Result<()> {
        loop {
            enter_wait(&mut self.ring, c)?;
            let mut ready = false;
            for cqe in self.ring.completions() {
                if cqe.flags & IORING_CQE_F_MORE == 0 {
                    self.armed = false;
                }
                if cqe.res < 0 {
                    return Err(io::Error::from_raw_os_error(-cqe.res));
                }
                ready = true;
            }
            if !self.armed {
                self.arm()?;
            }
            if ready {
                return Ok(());
            }
        }
    }
}

impl Drop for UringPoll {
    fn drop(&mut self) {
        if self.armed
            && let Err(e) = cancel_and_reap(&mut self.ring, RECV_UD)
        {
            eprintln!("read_path: poll teardown failed: {e}");
        }
    }
}

/// Path C: edge-triggered epoll.
pub struct EpollEt {
    pub epfd: OwnedFd,
}

impl EpollEt {
    pub fn new(fd: RawFd) -> io::Result<Self> {
        // SAFETY: no pointer arguments; a non-negative result is a new fd nothing else owns.
        let epfd = unsafe { OwnedFd::from_raw_fd(cvt(libc::epoll_create1(libc::EPOLL_CLOEXEC))?) };
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLET) as u32,
            u64: 0,
        };
        // SAFETY: `ev` is a valid epoll_event and both fds are open.
        cvt(unsafe { libc::epoll_ctl(epfd.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut ev) })?;
        Ok(Self { epfd })
    }
}

impl Readiness for EpollEt {
    fn wait(&mut self, c: &mut Counters) -> io::Result<()> {
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        loop {
            c.epoll_wait += 1;
            // SAFETY: `ev` is room for the one event asked for.
            let n = unsafe { libc::epoll_wait(self.epfd.as_raw_fd(), &mut ev, 1, -1) };
            if n > 0 {
                return Ok(());
            }
            if n < 0 && io::Error::last_os_error().kind() != ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
    }
}

/// Paths B and C: `recv` straight into the caller's buffer, until `EAGAIN`, then wait again.
pub struct Direct<W> {
    pub fd: RawFd,
    pub wait: W,
    pub ready: bool,
    pub c: Counters,
}

impl<W: Readiness> Direct<W> {
    pub fn new(fd: RawFd, wait: W) -> Self {
        // Ready at first: the first read tries the socket before any readiness event exists.
        Self {
            fd,
            wait,
            ready: true,
            c: Counters::default(),
        }
    }

    pub fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        loop {
            if !self.ready {
                self.wait.wait(&mut self.c)?;
                self.ready = true;
            }
            self.c.recv += 1;
            // SAFETY: `dst` is valid for `dst.len()` writable bytes.
            let n = unsafe { libc::recv(self.fd, dst.as_mut_ptr().cast(), dst.len(), 0) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                ErrorKind::WouldBlock => self.ready = false,
                ErrorKind::Interrupted => {}
                _ => return Err(e),
            }
        }
    }
}

pub enum Reader {
    Pool(Box<PoolReader>),
    Uring(Direct<UringPoll>),
    Epoll(Direct<EpollEt>),
}

impl Reader {
    pub fn new(path: Path, fd: RawFd, cfg: PoolCfg) -> io::Result<Self> {
        Ok(match path {
            Path::Pool => Self::Pool(Box::new(PoolReader::new(fd, cfg)?)),
            Path::UringPoll => Self::Uring(Direct::new(fd, UringPoll::new(fd)?)),
            Path::Epoll => Self::Epoll(Direct::new(fd, EpollEt::new(fd)?)),
        })
    }

    #[inline]
    pub fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Pool(r) => r.read(dst),
            Self::Uring(r) => r.read(dst),
            Self::Epoll(r) => r.read(dst),
        }
    }

    pub fn counters(&mut self) -> &mut Counters {
        match self {
            Self::Pool(r) => &mut r.c,
            Self::Uring(r) => &mut r.c,
            Self::Epoll(r) => &mut r.c,
        }
    }
}

/// Whether this kernel gives provided buffer rings (some distribution kernels do not); a refusal
/// skips (or fails when io_uring is required).
pub fn buffer_rings_available() -> bool {
    let refused =
        Ring::new(&RingConfig::new(8)).and_then(|ring| BufRing::new(&ring, 0, 8, 4096).map(drop));
    match refused {
        Ok(()) => true,
        Err(e) if env::var("NUCLEI_REQUIRE_IO_URING").as_deref() == Ok("1") => {
            panic!("io_uring is required but has no buffer rings: {e}")
        }
        Err(e) => {
            eprintln!("skipping: no provided buffer rings: {e}");
            false
        }
    }
}

/// Whether this machine can run io_uring; a refusal skips (or fails when io_uring is required).
pub fn uring_available() -> bool {
    match Ring::new(&RingConfig::new(8)) {
        Ok(_) => true,
        Err(e) if env::var("NUCLEI_REQUIRE_IO_URING").as_deref() == Ok("1") => {
            panic!("io_uring is required but unavailable: {e}")
        }
        Err(e) => {
            eprintln!("skipping: io_uring unavailable: {e}");
            false
        }
    }
}
