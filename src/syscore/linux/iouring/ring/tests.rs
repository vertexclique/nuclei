//! Ring-layer tests against the real kernel. Every test that needs a ring goes through
//! [`ring_or_skip`], so a machine without io_uring skips them unless `NUCLEI_REQUIRE_IO_URING=1`.

use std::{
    collections::VecDeque,
    env,
    ffi::CString,
    fs::{self, OpenOptions},
    io::{self, ErrorKind},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use super::{buf_ring::BufRing, *};

/// Panics when io_uring is required (`NUCLEI_REQUIRE_IO_URING=1`), otherwise prints a skip line.
pub(super) fn unavailable<T>(e: io::Error) -> Option<T> {
    if env::var("NUCLEI_REQUIRE_IO_URING").as_deref() == Ok("1") {
        panic!("io_uring is required but unavailable: {e}");
    }
    eprintln!("skipping: io_uring unavailable: {e}");
    None
}

/// Creates a ring from `cfg`, or skips the calling test when the kernel cannot provide one.
pub(super) fn ring_with_or_skip(cfg: RingConfig) -> Option<Ring> {
    Ring::new(&cfg).map_or_else(unavailable, Some)
}

/// A 64-entry ring with the default policy, or `None` to skip.
pub(super) fn ring_or_skip() -> Option<Ring> {
    ring_with_or_skip(RingConfig::new(64))
}

/// Runs `f` until it returns `Some`, backing off between tries, and panics at the deadline.
pub(super) fn wait_until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(50);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
}

/// Completions already taken off the ring but not yet looked at, so a test can take them one by one.
#[derive(Default)]
pub(super) struct Events(VecDeque<IoUringCqe>);

impl Events {
    /// The next completion, submitting and waiting for it if none is queued.
    pub(super) fn next(&mut self, ring: &mut Ring) -> IoUringCqe {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(cqe) = self.0.pop_front() {
                return cqe;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a completion"
            );
            match ring.submit_and_wait(1, Some(Duration::from_millis(200))) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => panic!("submit_and_wait failed: {e}"),
            }
            self.0.extend(ring.completions());
        }
    }

    /// The next `n` completions, in arrival order.
    pub(super) fn take(&mut self, ring: &mut Ring, n: usize) -> Vec<IoUringCqe> {
        (0..n).map(|_| self.next(ring)).collect()
    }
}

/// Submits what is queued and returns exactly `n` completions, sorted by `user_data`.
pub(super) fn reap(ring: &mut Ring, n: usize) -> Vec<IoUringCqe> {
    let mut events = Events::default();
    let mut cqes = events.take(ring, n);
    assert!(
        events.0.is_empty(),
        "unexpected extra completions: {:?}",
        events.0
    );
    cqes.sort_by_key(|c| c.user_data);
    cqes
}

/// Pushes `sqe`, failing the test if the queue is full.
pub(super) fn push(ring: &mut Ring, sqe: Sqe) {
    ring.push(&sqe).expect("submission queue has room");
}

/// A unique path in the system temp dir, removed on drop.
pub(super) struct TempPath(pub(super) PathBuf);

impl TempPath {
    pub(super) fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(env::temp_dir().join(format!("nuclei-ring-{}-{tag}-{n}", std::process::id())))
    }

    pub(super) fn cstring(&self) -> CString {
        path_cstring(&self.0)
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn path_cstring(p: &Path) -> CString {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes()).expect("temp path has no NUL")
}

/// An all-zero value of a plain C struct from `libc`.
pub(super) fn zeroed<T>() -> T {
    // SAFETY: only used with libc POD structs (statx, siginfo_t, msghdr, sockaddr_*), for which
    // the all-zero bit pattern is a valid value.
    unsafe { std::mem::zeroed() }
}

/// A connected `AF_UNIX` stream socket pair.
pub(super) fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors the call writes.
    let r = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    assert_eq!(r, 0, "socketpair: {}", io::Error::last_os_error());
    // SAFETY: socketpair returned two fresh descriptors that nothing else owns.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Writes all of `data` to `fd` with a plain `write(2)`.
pub(super) fn write_fd(fd: RawFd, data: &[u8]) {
    // SAFETY: `data` is a valid readable buffer of the given length.
    let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
    assert_eq!(
        n,
        data.len() as isize,
        "write: {}",
        io::Error::last_os_error()
    );
}

// ---------------------------------------------------------------------------------------------
// A Ring belongs to one thread: it is neither Send nor Sync. These items stop compiling if a
// future change makes either type thread-safe (the type annotation becomes ambiguous).

trait AmbiguousIfSend<A> {
    fn some_item() {}
}
impl<T: ?Sized> AmbiguousIfSend<()> for T {}
impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

trait AmbiguousIfSync<A> {
    fn some_item() {}
}
impl<T: ?Sized> AmbiguousIfSync<()> for T {}
impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}

const _: fn() = || {
    let _ = <Ring as AmbiguousIfSend<_>>::some_item;
    let _ = <Ring as AmbiguousIfSync<_>>::some_item;
    let _ = <BufRing as AmbiguousIfSend<_>>::some_item;
    let _ = <BufRing as AmbiguousIfSync<_>>::some_item;
};

// ---------------------------------------------------------------------------------------------
// Submission and completion queues.

#[test]
fn nop_round_trip_echoes_user_data() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    for ud in [0, 1, 0xdead_beef, u64::MAX] {
        push(&mut ring, Sqe::nop().user_data(ud));
        assert_eq!(ring.submit_and_wait(1, None).unwrap(), 1);
        let got: Vec<_> = ring.completions().collect();
        assert_eq!(
            got,
            [IoUringCqe {
                user_data: ud,
                res: 0,
                flags: 0
            }]
        );
    }
}

#[test]
fn queues_wrap_many_times_without_losing_an_entry() {
    let Some(mut ring) = ring_with_or_skip(RingConfig::new(8)) else {
        return;
    };
    let mut next = 0u64;
    for _ in 0..100 {
        let first = next;
        while ring.push(&Sqe::nop().user_data(next)).is_ok() {
            next += 1;
        }
        assert_eq!(next - first, 8);
        assert_eq!(ring.push(&Sqe::nop()), Err(SqFull));
        let cqes = reap(&mut ring, 8);
        let ids: Vec<u64> = cqes.iter().map(|c| c.user_data).collect();
        assert_eq!(ids, (first..next).collect::<Vec<_>>());
        assert!(cqes.iter().all(|c| c.res == 0));
    }
    assert_eq!(ring.sq_space(), 8);
}

#[test]
fn dropping_a_partly_read_iterator_keeps_the_rest() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    for i in 0..4 {
        push(&mut ring, Sqe::nop().user_data(i));
    }
    ring.submit_and_wait(4, None).unwrap();
    {
        let mut it = ring.completions();
        assert_eq!(it.len(), 4);
        assert_eq!(it.next().map(|c| c.user_data), Some(0));
        assert_eq!(it.next().map(|c| c.user_data), Some(1));
    }
    let rest: Vec<u64> = ring.completions().map(|c| c.user_data).collect();
    assert_eq!(rest, [2, 3]);
    assert_eq!(ring.completions().len(), 0);
}

#[test]
fn a_ring_with_nothing_queued_does_not_wait_for_nothing() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    assert_eq!(ring.submit().unwrap(), 0);
    assert_eq!(ring.completions().len(), 0);
}

#[test]
fn timed_wait_returns_ok_with_no_completions() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let start = Instant::now();
    assert_eq!(
        ring.submit_and_wait(1, Some(Duration::from_millis(50)))
            .unwrap(),
        0
    );
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(45), "returned after {took:?}");
    assert!(took < Duration::from_secs(5), "returned after {took:?}");
    assert_eq!(ring.completions().len(), 0);
}

#[test]
fn timed_wait_still_reports_what_was_submitted_and_completed() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    push(&mut ring, Sqe::nop().user_data(1));
    let took = ring
        .submit_and_wait(2, Some(Duration::from_millis(30)))
        .unwrap();
    assert_eq!(took, 1);
    let got: Vec<u64> = ring.completions().map(|c| c.user_data).collect();
    assert_eq!(got, [1]);
}

extern "C" fn on_signal(_: libc::c_int) {}

#[test]
fn an_interrupted_wait_surfaces_as_interrupted() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    // A handler without SA_RESTART: a signal ends the blocked wait with EINTR.
    let mut act: libc::sigaction = zeroed();
    act.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
    // SAFETY: `act` is a valid sigaction naming a handler that does nothing.
    assert_eq!(
        unsafe { libc::sigaction(libc::SIGUSR2, &act, ptr::null_mut()) },
        0
    );
    // SAFETY: pthread_self has no preconditions.
    let waiter = unsafe { libc::pthread_self() } as usize;
    let stop = Arc::new(AtomicBool::new(false));
    let poker = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // SAFETY: the waiting thread outlives the poker, which is joined below.
                unsafe { libc::pthread_kill(waiter as libc::pthread_t, libc::SIGUSR2) };
                thread::sleep(Duration::from_millis(2));
            }
        })
    };
    // The signal can land before the wait starts, so the poker keeps going until one interrupts it.
    let r = ring.submit_and_wait(1, Some(Duration::from_secs(10)));
    stop.store(true, Ordering::Relaxed);
    poker.join().unwrap();
    assert_eq!(r.unwrap_err().kind(), ErrorKind::Interrupted);
}

#[test]
fn enter_results_map_to_the_caller_facing_outcome() {
    let os = |code| Err(io::Error::from_raw_os_error(code));
    assert_eq!(enter_outcome(Ok(3)).unwrap(), 3);
    assert_eq!(enter_outcome(os(libc::ETIME)).unwrap(), 0);
    let lost = enter_outcome(os(libc::EBADR)).unwrap_err();
    assert!(lost.to_string().contains("lost"), "{lost}");
    assert_eq!(
        enter_outcome(os(libc::EINTR)).unwrap_err().kind(),
        ErrorKind::Interrupted
    );
    assert_eq!(
        enter_outcome(os(libc::EBUSY)).unwrap_err().raw_os_error(),
        Some(libc::EBUSY)
    );
}

#[test]
fn registered_ring_fd_serves_enters() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    ring.register_ring_fd().unwrap();
    let index = ring.reg_index;
    assert!(index.is_some());
    ring.register_ring_fd().unwrap();
    assert_eq!(
        ring.reg_index, index,
        "a second registration changes nothing"
    );
    for i in 0..3 {
        push(&mut ring, Sqe::nop().user_data(i));
        ring.submit_and_wait(1, Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(ring.completions().count(), 1);
    }
}

#[test]
fn completions_overflow_is_flushed_without_loss() {
    let Some(mut ring) = ring_with_or_skip(RingConfig::new(4)) else {
        return;
    };
    assert_eq!(ring.cq_entries(), 8);
    const TOTAL: u64 = 64;
    let mut next = 0;
    // Flood without reaping: only 8 completions fit, the rest sit in the kernel's backlog.
    while next < TOTAL {
        while next < TOTAL && ring.push(&Sqe::nop().user_data(next)).is_ok() {
            next += 1;
        }
        match ring.submit() {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
            Err(e) => panic!("submit failed: {e}"),
        }
    }
    assert!(
        ring.cq_overflow_pending(),
        "the flood should have overflowed the queue"
    );

    let mut seen = vec![0u32; TOTAL as usize];
    let mut got = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while got < TOTAL {
        assert!(
            Instant::now() < deadline,
            "only {got} of {TOTAL} completions arrived"
        );
        for cqe in ring.completions() {
            seen[cqe.user_data as usize] += 1;
            got += 1;
        }
        if got < TOTAL {
            ring.submit_and_wait(1, Some(Duration::from_millis(100)))
                .unwrap();
        }
    }
    assert!(
        seen.iter().all(|&n| n == 1),
        "every completion exactly once: {seen:?}"
    );
    assert!(!ring.cq_overflow_pending());
}

// ---------------------------------------------------------------------------------------------
// Files.

#[test]
fn write_then_read_at_explicit_offsets() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let path = TempPath::new("rw");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path.0)
        .unwrap();
    let fd = file.as_raw_fd();

    let (a, b) = (*b"hello ring", *b"offset 4096");
    push(
        &mut ring,
        Sqe::write(fd, a.as_ptr(), a.len() as u32, 0).user_data(1),
    );
    push(
        &mut ring,
        Sqe::write(fd, b.as_ptr(), b.len() as u32, 4096).user_data(2),
    );
    let cqes = reap(&mut ring, 2);
    assert_eq!([cqes[0].res, cqes[1].res], [10, 11]);

    let on_disk = fs::read(&path.0).unwrap();
    assert_eq!(on_disk.len(), 4096 + 11);
    assert_eq!(&on_disk[..10], &a);
    assert_eq!(&on_disk[4096..], &b);
    assert!(on_disk[10..4096].iter().all(|&x| x == 0));

    let (mut ra, mut rb, mut rc) = ([0u8; 10], [0u8; 11], [0u8; 8]);
    push(
        &mut ring,
        Sqe::read(fd, ra.as_mut_ptr(), 10, 0).user_data(1),
    );
    push(
        &mut ring,
        Sqe::read(fd, rb.as_mut_ptr(), 11, 4096).user_data(2),
    );
    push(
        &mut ring,
        Sqe::read(fd, rc.as_mut_ptr(), 8, 8192).user_data(3),
    );
    let cqes = reap(&mut ring, 3);
    assert_eq!([cqes[0].res, cqes[1].res, cqes[2].res], [10, 11, 0]);
    assert_eq!((ra, rb), (a, b));
}

#[test]
fn readv_and_writev_at_an_offset() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let path = TempPath::new("vec");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path.0)
        .unwrap();
    let fd = file.as_raw_fd();

    let (p, q) = (*b"abc", *b"defg");
    let out = [
        libc::iovec {
            iov_base: p.as_ptr() as *mut _,
            iov_len: p.len(),
        },
        libc::iovec {
            iov_base: q.as_ptr() as *mut _,
            iov_len: q.len(),
        },
    ];
    push(
        &mut ring,
        Sqe::writev(fd, out.as_ptr(), 2, 100).user_data(1),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, 7);
    let on_disk = fs::read(&path.0).unwrap();
    assert_eq!(&on_disk[100..], b"abcdefg");

    let (mut x, mut y) = ([0u8; 2], [0u8; 9]);
    let inp = [
        libc::iovec {
            iov_base: x.as_mut_ptr().cast(),
            iov_len: x.len(),
        },
        libc::iovec {
            iov_base: y.as_mut_ptr().cast(),
            iov_len: y.len(),
        },
    ];
    push(&mut ring, Sqe::readv(fd, inp.as_ptr(), 2, 100).user_data(2));
    assert_eq!(reap(&mut ring, 1)[0].res, 7);
    assert_eq!(&x, b"ab");
    assert_eq!(&y[..5], b"cdefg");
}

#[test]
fn openat_statx_and_close() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let path = TempPath::new("stat");
    fs::write(&path.0, vec![7u8; 12_345]).unwrap();
    let c = path.cstring();

    push(
        &mut ring,
        Sqe::openat(
            libc::AT_FDCWD,
            c.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC,
            0,
        )
        .user_data(1),
    );
    let fd = reap(&mut ring, 1)[0].res;
    assert!(fd >= 0, "openat failed with {fd}");

    let mut stx: libc::statx = zeroed();
    push(
        &mut ring,
        Sqe::statx(libc::AT_FDCWD, c.as_ptr(), 0, libc::STATX_SIZE, &mut stx).user_data(2),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, 0);
    assert_eq!(stx.stx_size, 12_345);

    // The opened fd is a real, readable descriptor of that file.
    let mut head = [0u8; 4];
    push(
        &mut ring,
        Sqe::read(fd, head.as_mut_ptr(), 4, 0).user_data(3),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, 4);
    assert_eq!(head, [7; 4]);

    push(&mut ring, Sqe::close(fd).user_data(4));
    assert_eq!(reap(&mut ring, 1)[0].res, 0);

    let missing = CString::new("/nonexistent-nuclei-ring-path").unwrap();
    push(
        &mut ring,
        Sqe::openat(libc::AT_FDCWD, missing.as_ptr(), libc::O_RDONLY, 0).user_data(5),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, -libc::ENOENT);
}

#[test]
fn fsync_and_fdatasync_complete() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let path = TempPath::new("sync");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path.0)
        .unwrap();
    let data = [1u8; 512];
    push(
        &mut ring,
        Sqe::write(file.as_raw_fd(), data.as_ptr(), 512, 0).user_data(1),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, 512);
    push(&mut ring, Sqe::fsync(file.as_raw_fd(), false).user_data(2));
    push(&mut ring, Sqe::fsync(file.as_raw_fd(), true).user_data(3));
    let cqes = reap(&mut ring, 2);
    assert_eq!([cqes[0].res, cqes[1].res], [0, 0]);
}

// ---------------------------------------------------------------------------------------------
// Timeouts and cancellation.

/// The time `ms` milliseconds from now on `clock`.
fn after(clock: libc::clockid_t, ms: i64) -> KernelTimespec {
    let mut ts: libc::timespec = zeroed();
    // SAFETY: `ts` is a valid timespec to write into.
    assert_eq!(unsafe { libc::clock_gettime(clock, &mut ts) }, 0);
    let nanos = ts.tv_nsec + ms * 1_000_000;
    KernelTimespec {
        tv_sec: ts.tv_sec + nanos / 1_000_000_000,
        tv_nsec: nanos % 1_000_000_000,
    }
}

#[test]
fn relative_and_absolute_timeouts_fire() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let rel = KernelTimespec::from_duration(Duration::from_millis(20));
    let abs = after(libc::CLOCK_MONOTONIC, 20);
    let real = after(libc::CLOCK_REALTIME, 20);
    let start = Instant::now();
    push(&mut ring, Sqe::timeout(&rel).user_data(1));
    push(&mut ring, Sqe::timeout_abs(&abs, 0).user_data(2));
    push(
        &mut ring,
        Sqe::timeout_abs(&real, IORING_TIMEOUT_REALTIME).user_data(3),
    );
    let cqes = reap(&mut ring, 3);
    assert!(cqes.iter().all(|c| c.res == -libc::ETIME), "{cqes:?}");
    assert!(start.elapsed() >= Duration::from_millis(15));
}

#[test]
fn timeout_can_be_removed() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let long = KernelTimespec::from_duration(Duration::from_secs(600));
    push(&mut ring, Sqe::timeout(&long).user_data(1));
    ring.submit().unwrap();
    push(&mut ring, Sqe::timeout_remove(1).user_data(2));
    let cqes = reap(&mut ring, 2);
    assert_eq!((cqes[0].res, cqes[1].res), (-libc::ECANCELED, 0));
}

#[test]
fn cancel_by_user_data_ends_a_pending_recv() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let (_a, b) = socketpair();
    let mut buf = [0u8; 16];
    push(
        &mut ring,
        Sqe::recv(b.as_raw_fd(), buf.as_mut_ptr(), 16, 0).user_data(1),
    );
    ring.submit().unwrap();
    push(&mut ring, Sqe::async_cancel(1).user_data(2));
    let cqes = reap(&mut ring, 2);
    assert_eq!((cqes[0].user_data, cqes[0].res), (1, -libc::ECANCELED));
    assert_eq!((cqes[1].user_data, cqes[1].res), (2, 0));
}

#[test]
fn cancel_of_nothing_reports_enoent() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    push(&mut ring, Sqe::async_cancel(999).user_data(1));
    assert_eq!(reap(&mut ring, 1)[0].res, -libc::ENOENT);
}

#[test]
fn cancel_by_fd_with_all_ends_every_recv_on_it() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let (_a, b) = socketpair();
    let mut bufs = [[0u8; 8]; 3];
    for (i, buf) in bufs.iter_mut().enumerate() {
        push(
            &mut ring,
            Sqe::recv(b.as_raw_fd(), buf.as_mut_ptr(), 8, 0).user_data(10 + i as u64),
        );
    }
    ring.submit().unwrap();
    push(
        &mut ring,
        Sqe::async_cancel_fd(b.as_raw_fd(), true).user_data(20),
    );
    let cqes = reap(&mut ring, 4);
    for c in &cqes[..3] {
        assert_eq!(c.res, -libc::ECANCELED, "{c:?}");
    }
    assert_eq!((cqes[3].user_data, cqes[3].res), (20, 3));
}

#[test]
fn cancel_any_with_all_ends_every_pending_op() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let (_a, b) = socketpair();
    let mut bufs = [[0u8; 8]; 2];
    for (i, buf) in bufs.iter_mut().enumerate() {
        push(
            &mut ring,
            Sqe::recv(b.as_raw_fd(), buf.as_mut_ptr(), 8, 0).user_data(1 + i as u64),
        );
    }
    ring.submit().unwrap();
    push(&mut ring, Sqe::async_cancel_any(true).user_data(9));
    let cqes = reap(&mut ring, 3);
    assert_eq!([cqes[0].res, cqes[1].res], [-libc::ECANCELED; 2]);
    assert_eq!((cqes[2].user_data, cqes[2].res), (9, 2));
}

// ---------------------------------------------------------------------------------------------
// Messages between rings.

#[test]
fn msg_ring_reaches_a_ring_on_another_thread() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let (fd_tx, fd_rx) = mpsc::channel::<Option<RawFd>>();
    let receiver = thread::spawn(move || {
        let Some(mut target) = ring_or_skip() else {
            fd_tx.send(None).unwrap();
            return None;
        };
        fd_tx.send(Some(target.as_raw_fd())).unwrap();
        Some(Events::default().next(&mut target))
    });
    let Some(target_fd) = fd_rx.recv().unwrap() else {
        assert!(receiver.join().unwrap().is_none());
        return;
    };
    push(&mut ring, Sqe::msg_ring(target_fd, 0xfeed, 42).user_data(7));
    let sent = reap(&mut ring, 1);
    assert_eq!((sent[0].user_data, sent[0].res), (7, 0));
    let got = receiver.join().unwrap().expect("target ring was created");
    assert_eq!((got.user_data, got.res), (0xfeed, 42));
}

// ---------------------------------------------------------------------------------------------
// NOP round trips through the ring, measured, to compare with `benches/reference`.

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

#[test]
#[ignore = "measurement: run with --release -- --ignored --nocapture bench_nop_round_trip"]
fn bench_nop_round_trip() {
    const OPS: u64 = 200_000;
    const RUNS: usize = 7;
    let Some(mut ring) = ring_with_or_skip(RingConfig::new(256)) else {
        return;
    };
    for batch in [1u32, 32] {
        let mut per_op = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let start = Instant::now();
            let (mut done, mut id) = (0u64, 0u64);
            while done < OPS {
                for _ in 0..batch {
                    ring.push(&Sqe::nop().user_data(id))
                        .expect("room for a batch");
                    id += 1;
                }
                ring.submit_and_wait(batch, None).expect("submit_and_wait");
                for cqe in ring.completions() {
                    assert_eq!(cqe.res, 0);
                    done += 1;
                }
            }
            per_op.push(start.elapsed().as_nanos() as f64 / OPS as f64);
        }
        println!(
            "nop batch={batch} ops={OPS} runs={RUNS} median_ns_per_op={:.1}",
            median(per_op)
        );
    }
}
