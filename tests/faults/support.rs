//! What every fault case shares: a runtime, deadlines, the bytes a case
//! sends and how it checks them, the byte streams as one trait, a case run
//! alone in its own process with its descriptors, memory and threads counted
//! before and after, and a future polled a given number of times and then
//! dropped.

#![allow(dead_code)]

use core::future::{Future, poll_fn};
use core::pin::{Pin, pin};
use core::task::{Context, Poll, Waker};
use std::io::{self, IoSlice, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Wake;
use std::time::{Duration, Instant};

use futures_io::{AsyncRead, AsyncWrite};
use nuclei::Runtime;
use nuclei::net::{TcpListener, TcpStream};

/// How long any one step may take before the case fails as hung: long, as
/// the host may be busy, and never reached by a case that works.
pub const DEADLINE: Duration = Duration::from_secs(60);

/// A runtime of two workers whose idle blocking threads go at once, so a
/// count of threads sees only those that work.
pub fn runtime() -> Runtime {
    Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .thread_keep_alive(Duration::from_millis(100))
        .build()
        .unwrap()
}

/// `fut`, or a failure naming `what` if it takes longer than [`DEADLINE`].
pub async fn within<F: Future>(what: &str, fut: F) -> F::Output {
    match nuclei::time::timeout(DEADLINE, fut).await {
        Ok(out) => out,
        Err(_) => panic!("{what}: no answer in {DEADLINE:?}"),
    }
}

/// Waits in a task until `done` holds, with a growing pause, and fails
/// naming `what` past [`DEADLINE`]; the shard runs its other work meanwhile.
pub async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    let mut pause = Duration::from_millis(1);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "{what}: not so after {DEADLINE:?}"
        );
        nuclei::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(50));
    }
}

/// Waits on a plain thread until `done` holds, with a growing pause, and
/// fails naming `what` past [`DEADLINE`].
pub fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    let mut pause = Duration::from_millis(1);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "{what}: not so after {DEADLINE:?}"
        );
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(50));
    }
}

/// `len` bytes that differ with `seed` and with every position.
pub fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Asserts `got` is `want`, naming the first byte that differs rather than
/// printing megabytes.
pub fn same_bytes(what: &str, got: &[u8], want: &[u8]) {
    if got == want {
        return;
    }
    let at = got.iter().zip(want).position(|(a, b)| a != b);
    panic!(
        "{what}: {} bytes where {} were sent, first difference at {at:?}",
        got.len(),
        want.len()
    );
}

/// An answer as the cases compare them: a count, or an error's kind and code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Ok(usize),
    Err(io::ErrorKind, Option<i32>),
}

impl Answer {
    pub fn of<T>(result: &io::Result<T>, n: impl FnOnce(&T) -> usize) -> Self {
        match result {
            Ok(v) => Answer::Ok(n(v)),
            Err(err) => Answer::Err(err.kind(), err.raw_os_error()),
        }
    }

    pub fn kind(&self) -> Option<io::ErrorKind> {
        match self {
            Answer::Ok(_) => None,
            Answer::Err(kind, _) => Some(*kind),
        }
    }
}

/// What a write to a stream whose peer reset may answer, as std's does.
pub const RESET_WRITE: &[io::ErrorKind] = &[
    io::ErrorKind::ConnectionReset,
    io::ErrorKind::BrokenPipe,
    io::ErrorKind::ConnectionAborted,
];

/// What a read on a stream whose peer reset may answer, as std's does.
pub const RESET_READ: &[io::ErrorKind] = &[
    io::ErrorKind::ConnectionReset,
    io::ErrorKind::ConnectionAborted,
];

pub fn is_one_of(err: &io::Error, kinds: &[io::ErrorKind]) -> bool {
    kinds.contains(&err.kind())
}

/// A byte stream of nuclei's, with its std counterpart for the peer and the
/// oracle: TCP everywhere, Unix streams on unix.
pub trait Stream: Sized + Send + Sync + 'static {
    type Std: Read + Write + Send + Sync + 'static;
    const NAME: &'static str;

    /// Our end and the peer's, connected; with `tiny`, the peer's buffers
    /// and ours are as small as the system allows.
    fn pair(tiny: bool) -> (Self, Self::Std);
    /// Two std ends, connected, for the oracle.
    fn std_pair(tiny: bool) -> (Self::Std, Self::Std);
    /// Wraps an std end, set up as `pair` would.
    fn adopt(std: Self::Std) -> Self;
    /// Closes the peer's end so that it resets: TCP with `SO_LINGER` 0; a
    /// Unix stream with bytes it never read, which its peer sees as a reset.
    fn reset(peer: Self::Std);
    fn shutdown_std(s: &Self::Std, how: std::net::Shutdown) -> io::Result<()>;
    fn set_std_timeouts(s: &Self::Std);
    /// Whether closing with unread bytes resets the stream on this system.
    fn resets() -> bool;

    fn poll_read(&self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>>;
    fn poll_read_vectored(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>>;
    fn poll_write(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>>;
    fn poll_write_vectored(
        &self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>>;
    fn poll_flush(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
    fn poll_close(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>>;
    fn try_write(&self, buf: &[u8]) -> io::Result<usize>;
    fn read_owned(
        &self,
        buf: Vec<u8>,
    ) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> + Send + '_;
    fn write_owned(
        &self,
        buf: Vec<u8>,
    ) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> + Send + '_;
    fn send_file<'a>(
        &'a self,
        file: &'a nuclei::fs::File,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = io::Result<u64>> + Send + 'a;
}

macro_rules! stream_calls {
    () => {
        fn poll_read(&self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
            Pin::new(&mut &*self).poll_read(cx, buf)
        }
        fn poll_read_vectored(
            &self,
            cx: &mut Context<'_>,
            bufs: &mut [io::IoSliceMut<'_>],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut &*self).poll_read_vectored(cx, bufs)
        }
        fn poll_write(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            Pin::new(&mut &*self).poll_write(cx, buf)
        }
        fn poll_write_vectored(
            &self,
            cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut &*self).poll_write_vectored(cx, bufs)
        }
        fn poll_flush(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut &*self).poll_flush(cx)
        }
        fn poll_close(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut &*self).poll_close(cx)
        }
        fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
            Self::try_write(self, buf)
        }
        fn read_owned(
            &self,
            buf: Vec<u8>,
        ) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> + Send + '_ {
            Self::read_owned(self, buf)
        }
        fn write_owned(
            &self,
            buf: Vec<u8>,
        ) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> + Send + '_ {
            Self::write_owned(self, buf)
        }
        fn send_file<'a>(
            &'a self,
            file: &'a nuclei::fs::File,
            offset: u64,
            len: u64,
        ) -> impl Future<Output = io::Result<u64>> + Send + 'a {
            Self::send_file(self, file, offset, len)
        }
    };
}

/// The socket buffers a case of small buffers asks for: for a Unix socket
/// the least the system allows (it rounds 1 up); for TCP a few pages, since
/// at its least the kernel moves a MiB in a minute on loopback, std's sockets
/// as much as nuclei's.
pub const TINY: u32 = 1;
pub const TINY_TCP: u32 = 8192;

/// A connected TCP pair of std's, both ends' buffers tiny with `tiny`.
fn std_tcp(tiny: bool) -> (std::net::TcpStream, std::net::TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    #[cfg(windows)]
    let ours = if tiny {
        // Set before the connect, through nuclei's socket: std has no option
        // for it here.
        let socket = nuclei::net::TcpSocket::new_v4().unwrap();
        socket.set_send_buffer_size(TINY_TCP).unwrap();
        socket.set_recv_buffer_size(TINY_TCP).unwrap();
        let socket = nuclei::block_on(socket.connect(addr)).unwrap();
        let std = socket.into_inner();
        std.set_nonblocking(false).unwrap();
        std
    } else {
        std::net::TcpStream::connect(addr).unwrap()
    };
    #[cfg(unix)]
    let ours = std::net::TcpStream::connect(addr).unwrap();
    let (peer, _) = listener.accept().unwrap();
    if tiny {
        set_buffers(&ours, TINY_TCP);
        set_buffers(&peer, TINY_TCP);
    }
    (ours, peer)
}

/// Shrinks the buffers of an std socket to the least through its descriptor.
#[cfg(unix)]
pub fn set_tiny<S: std::os::fd::AsRawFd>(s: &S) {
    set_buffers(s, TINY);
}

/// Sets both buffers of an std socket to `size` through its descriptor.
#[cfg(unix)]
pub fn set_buffers<S: std::os::fd::AsRawFd>(s: &S, size: u32) {
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        let value: libc::c_int = size as libc::c_int;
        // SAFETY: a plain option set on a descriptor the caller holds, with
        // a value of the size given.
        let rc = unsafe {
            libc::setsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&value as *const libc::c_int).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "{}", io::Error::last_os_error());
    }
}

#[cfg(windows)]
fn set_buffers(s: &std::net::TcpStream, _: u32) {
    // The accepted end keeps the system's buffers here: Windows has no
    // option set of std's, and the end that connected is tiny already.
    let _ = s;
}

/// Sets `SO_LINGER` to 0 on an std TCP stream through a handle of nuclei's,
/// and gives the stream back blocking.
pub fn zero_linger(s: std::net::TcpStream) -> std::net::TcpStream {
    let handle = nuclei::Handle::new(s).unwrap();
    handle.set_zero_linger().unwrap();
    let s = handle.into_inner();
    s.set_nonblocking(false).unwrap();
    s
}

impl Stream for TcpStream {
    type Std = std::net::TcpStream;
    const NAME: &'static str = "tcp";

    fn pair(tiny: bool) -> (Self, Self::Std) {
        let (ours, peer) = std_tcp(tiny);
        Self::set_std_timeouts(&peer);
        (Self::adopt(ours), peer)
    }

    fn std_pair(tiny: bool) -> (Self::Std, Self::Std) {
        let (a, b) = std_tcp(tiny);
        Self::set_std_timeouts(&a);
        Self::set_std_timeouts(&b);
        (a, b)
    }

    fn adopt(std: Self::Std) -> Self {
        nuclei::Handle::new(std).unwrap()
    }

    fn reset(peer: Self::Std) {
        drop(zero_linger(peer));
    }

    fn shutdown_std(s: &Self::Std, how: std::net::Shutdown) -> io::Result<()> {
        s.shutdown(how)
    }

    fn set_std_timeouts(s: &Self::Std) {
        s.set_read_timeout(Some(DEADLINE)).unwrap();
        s.set_write_timeout(Some(DEADLINE)).unwrap();
    }

    fn resets() -> bool {
        true
    }

    stream_calls!();
}

#[cfg(unix)]
impl Stream for nuclei::net::UnixStream {
    type Std = std::os::unix::net::UnixStream;
    const NAME: &'static str = "unix";

    fn pair(tiny: bool) -> (Self, Self::Std) {
        let (ours, peer) = Self::std_pair(tiny);
        (Self::adopt(ours), peer)
    }

    fn std_pair(tiny: bool) -> (Self::Std, Self::Std) {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        if tiny {
            set_tiny(&a);
            set_tiny(&b);
        }
        Self::set_std_timeouts(&a);
        Self::set_std_timeouts(&b);
        (a, b)
    }

    fn adopt(std: Self::Std) -> Self {
        nuclei::Handle::new(std).unwrap()
    }

    fn reset(peer: Self::Std) {
        drop(peer);
    }

    fn shutdown_std(s: &Self::Std, how: std::net::Shutdown) -> io::Result<()> {
        s.shutdown(how)
    }

    fn set_std_timeouts(s: &Self::Std) {
        s.set_read_timeout(Some(DEADLINE)).unwrap();
        s.set_write_timeout(Some(DEADLINE)).unwrap();
    }

    fn resets() -> bool {
        // Linux and Android reset a Unix stream closed with bytes unread;
        // the BSDs and macOS end it.
        cfg!(any(target_os = "linux", target_os = "android"))
    }

    stream_calls!();
}

pub async fn read_some<S: Stream>(s: &S, buf: &mut [u8]) -> io::Result<usize> {
    poll_fn(|cx| s.poll_read(cx, buf)).await
}

pub async fn read_vectored<S: Stream>(s: &S, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
    poll_fn(|cx| s.poll_read_vectored(cx, bufs)).await
}

pub async fn write_some<S: Stream>(s: &S, buf: &[u8]) -> io::Result<usize> {
    poll_fn(|cx| s.poll_write(cx, buf)).await
}

pub async fn flush<S: Stream>(s: &S) -> io::Result<()> {
    poll_fn(|cx| s.poll_flush(cx)).await
}

pub async fn close<S: Stream>(s: &S) -> io::Result<()> {
    poll_fn(|cx| s.poll_close(cx)).await
}

/// A thread's result, waited for on the blocking pool, so the runtime's
/// thread keeps running its shard meanwhile: a thread that blocks inside a
/// task holds its shard's IO up.
pub async fn joined<T: Send + 'static>(thread: std::thread::JoinHandle<T>) -> T {
    match within(
        "a peer's thread",
        nuclei::spawn_blocking(move || thread.join()),
    )
    .await
    {
        Ok(out) => out,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Writes all of `buf` with plain writes, then flushes.
pub async fn write_all<S: Stream>(s: &S, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match write_some(s, buf).await? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => buf = &buf[n..],
        }
    }
    flush(s).await
}

/// Writes all of `buf` in vectored writes of `pieces` slices, then flushes.
pub async fn write_all_vectored<S: Stream>(s: &S, buf: &[u8], pieces: usize) -> io::Result<()> {
    let mut at = 0;
    while at < buf.len() {
        let rest = &buf[at..];
        let step = rest.len().div_ceil(pieces).max(1);
        let slices: Vec<IoSlice<'_>> = rest.chunks(step).map(IoSlice::new).collect();
        match poll_fn(|cx| s.poll_write_vectored(cx, &slices)).await? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => at += n,
        }
    }
    flush(s).await
}

/// Writes all of `buf` lent to the driver, in owned writes of `chunk`.
pub async fn write_all_owned<S: Stream>(s: &S, buf: &[u8], chunk: usize) -> io::Result<()> {
    for piece in buf.chunks(chunk.max(1)) {
        let (result, back) = s.write_owned(piece.to_vec()).await;
        assert_eq!(
            back.len(),
            piece.len(),
            "the owned write gave back another buffer"
        );
        assert_eq!(
            result?,
            piece.len(),
            "an owned write that ended took part of it"
        );
    }
    Ok(())
}

/// Reads until the end of the stream or an error, in reads of `chunk`:
/// what came, and the read that ended it.
pub async fn read_until_end<S: Stream>(s: &S, chunk: usize) -> (Vec<u8>, io::Result<usize>) {
    let mut got = Vec::new();
    let mut buf = vec![0u8; chunk.max(1)];
    loop {
        match read_some(s, &mut buf).await {
            Ok(0) => return (got, Ok(0)),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(err) => return (got, Err(err)),
        }
    }
}

/// As [`read_until_end`], with owned reads of `chunk` spare bytes.
pub async fn read_owned_until_end<S: Stream>(s: &S, chunk: usize) -> (Vec<u8>, io::Result<usize>) {
    let mut got = Vec::new();
    loop {
        let (result, buf) = s.read_owned(Vec::with_capacity(chunk.max(1))).await;
        match result {
            Ok(0) => return (got, Ok(0)),
            Ok(n) => {
                assert_eq!(buf.len(), n, "an owned read answered another count");
                got.extend_from_slice(&buf);
            }
            Err(err) => return (got, Err(err)),
        }
    }
}

/// Reads exactly `len` bytes, as std's `read_exact`.
pub async fn read_exact<S: Stream>(s: &S, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut at = 0;
    while at < len {
        match read_some(s, &mut buf[at..]).await? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => at += n,
        }
    }
    Ok(buf)
}

/// Reads an std stream until its end or an error.
pub fn std_read_until_end<R: Read>(r: &mut R, chunk: usize) -> (Vec<u8>, io::Result<usize>) {
    let mut got = Vec::new();
    let mut buf = vec![0u8; chunk.max(1)];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return (got, Ok(0)),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return (got, Err(err)),
        }
    }
}

/// A connected TCP pair of nuclei's, through its own connect and accept.
pub async fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (client, accepted) = nuclei::join!(TcpStream::connect(addr), listener.accept());
    (client.unwrap(), accepted.unwrap().0)
}

/// A word sent there and back on a new connection: the runtime still works.
pub async fn round_trip() {
    within("a round trip after the fault", async {
        let (a, b) = tcp_pair().await;
        write_all(&a, b"ping").await.unwrap();
        assert_eq!(read_exact(&b, 4).await.unwrap(), b"ping");
        write_all(&b, b"pong").await.unwrap();
        assert_eq!(read_exact(&a, 4).await.unwrap(), b"pong");
    })
    .await;
}

/// A file of the case's own under the system's temporary directory, removed
/// when this drops.
pub struct TempFile(pub std::path::PathBuf);

impl TempFile {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        Self(std::env::temp_dir().join(format!(
            "nuclei-faults-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    pub fn with(name: &str, bytes: &[u8]) -> Self {
        let file = Self::new(name);
        std::fs::write(&file.0, bytes).unwrap();
        file
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A waker that counts its wakes and passes each on to the task that polls.
#[derive(Default)]
struct Counting {
    wakes: AtomicUsize,
    outer: Mutex<Option<Waker>>,
}

impl Wake for Counting {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::AcqRel);
        if let Some(w) = self.outer.lock().unwrap().take() {
            w.wake();
        }
    }
}

/// When [`poll_times`] drops a future that has not completed.
#[derive(Clone, Copy, Debug)]
pub enum DropAt {
    /// Right after its last poll.
    Poll,
    /// After the wake that follows its last poll: the driver has done what
    /// the future waited for, and the future never saw it.
    Wake,
}

/// Polls `fut` at most `polls` times with a counting waker, the first at
/// once and each other after a wake of it, and drops it as `at` says: its
/// output if it completed within them. A future that waits with no wake to
/// come fails the case: it would hang.
pub async fn poll_times<F: Future>(
    what: &str,
    fut: F,
    polls: usize,
    at: DropAt,
) -> Option<F::Output> {
    let mut fut = pin!(fut);
    let count = Arc::new(Counting::default());
    let waker = Waker::from(Arc::clone(&count));
    let mut seen = 0;
    for i in 0..polls {
        if i > 0 {
            woken(what, &count, seen).await;
        }
        seen = count.wakes.load(Ordering::Acquire);
        if let Poll::Ready(out) = fut.as_mut().poll(&mut Context::from_waker(&waker)) {
            return Some(out);
        }
    }
    if polls > 0 && matches!(at, DropAt::Wake) {
        woken(what, &count, seen).await;
    }
    None
}

/// Waits until `count` has more wakes than `seen`.
async fn woken(what: &str, count: &Arc<Counting>, seen: usize) {
    within(
        &format!("{what}: a wake after {seen}"),
        poll_fn(|cx| {
            *count.outer.lock().unwrap() = Some(cx.waker().clone());
            if count.wakes.load(Ordering::Acquire) > seen {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }),
    )
    .await;
}

/// The descriptors (on Windows the handles) this process holds open.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn open_handles() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
pub fn open_handles() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

#[cfg(windows)]
pub fn open_handles() -> usize {
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessHandleCount(process: isize, count: *mut u32) -> i32;
    }
    let mut count = 0u32;
    // SAFETY: the pseudo handle of this process, and a place for the count.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    assert_ne!(ok, 0, "{}", io::Error::last_os_error());
    count as usize
}

/// The threads of this process, where the system lists them.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn threads() -> Option<usize> {
    Some(std::fs::read_dir("/proc/self/task").ok()?.count())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn threads() -> Option<usize> {
    None
}

/// Set in a process of this test binary that runs one case alone.
const ALONE: &str = "NUCLEI_FAULTS_ALONE";

/// Runs the test `name` again, alone in a new process of this binary, and
/// checks it ran and passed there. True in that process, where the caller
/// runs its case: descriptor and memory counts and resource limits are the
/// whole process's.
pub fn alone(name: &str) -> bool {
    if std::env::var_os(ALONE).is_some() {
        return true;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1", "--nocapture"])
        .env(ALONE, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = out.read_to_string(&mut text);
        let mut errs = String::new();
        let _ = err.read_to_string(&mut errs);
        text + &errs
    });
    let deadline = Instant::now() + DEADLINE * 5;
    let mut pause = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{name} did not finish in its own process");
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(100));
    };
    let out = reader.join().unwrap();
    assert!(status.success(), "{name} failed in its own process:\n{out}");
    assert!(
        out.contains("test result: ok. 1 passed"),
        "{name} did not run in its own process:\n{out}"
    );
    false
}

/// What the process holds: open descriptors, live heap bytes outside the
/// shards' buffer pools (which keep what they freed for the next IO, up to
/// their cap), threads.
#[derive(Clone, Copy, Debug)]
pub struct Held {
    pub handles: usize,
    pub bytes: isize,
    pub threads: Option<usize>,
}

impl Held {
    pub fn now() -> Self {
        let pools: usize = nuclei::stats().shards.iter().map(|s| s.buffer_bytes).sum();
        Self {
            handles: open_handles(),
            bytes: crate::live_bytes() - pools as isize,
            threads: threads(),
        }
    }
}

/// What may be live more after a case's rounds than after its warm-up:
/// nodes retired and not reclaimed yet.
pub const SLACK: isize = 64 * 1024;

/// Submits what every worker of `rt`, its blocking pool and this thread
/// retired to reclamation (see tests/steady.rs, whose count this follows).
pub fn reclaim(rt: &Runtime, workers: usize) {
    let mut seen = Vec::new();
    let deadline = Instant::now() + DEADLINE;
    while seen.len() < workers {
        assert!(
            Instant::now() < deadline,
            "the workers took no task to flush"
        );
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tasks: Vec<_> = (0..workers * 64)
            .map(|_| {
                let release = Arc::clone(&release);
                rt.spawn(async move {
                    kovan::flush();
                    let id = std::thread::current().id();
                    while !release.load(Ordering::Acquire) {
                        nuclei::task::yield_now().await;
                    }
                    id
                })
            })
            .collect();
        release.store(true, Ordering::Release);
        for task in tasks {
            let id = rt.block_on(task);
            if !seen.contains(&id) {
                seen.push(id);
            }
        }
    }
    rt.block_on(rt.spawn_blocking(kovan::flush));
    kovan::flush();
}

/// Runs `case` on a runtime of two workers twice to warm up, then `rounds`
/// times, then `rounds` times more, and checks that the second run of rounds
/// left the process holding no more descriptors, threads or heap bytes than
/// the first: a pool that grew to what the case needs stays that size, and a
/// leak grows every round. A round trip works at the end.
pub fn holds_steady<F, Fut>(name: &str, rounds: usize, mut case: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    // No handoff: a busy host that stops a worker in a poll past the
    // threshold would hand its shard over and start the runtime's first
    // spare thread, once, which is no round's growth (as in tests/steady.rs).
    let rt = Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .thread_keep_alive(Duration::from_millis(100))
        .handoff_after(None)
        .build()
        .unwrap();
    for _ in 0..2 {
        rt.block_on(within(name, case()));
    }
    rt.block_on(round_trip());
    let settle = |before: Option<Held>| {
        let deadline = Instant::now() + DEADLINE;
        let mut pause = Duration::from_millis(1);
        loop {
            reclaim(&rt, 2);
            let now = Held::now();
            let settled = before.is_none_or(|b| {
                now.handles <= b.handles && now.bytes - b.bytes <= SLACK && now.threads <= b.threads
            });
            if settled || Instant::now() > deadline {
                return now;
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(50));
        }
    };
    let mut held = Vec::new();
    for _ in 0..2 {
        for _ in 0..rounds {
            rt.block_on(within(name, case()));
        }
        held.push(settle(held.last().copied()));
    }
    rt.block_on(round_trip());
    let (before, after) = (held[0], held[1]);
    assert!(
        after.handles <= before.handles,
        "{name}: {} open descriptors after {rounds} rounds, {} after {rounds} more",
        before.handles,
        after.handles
    );
    assert!(
        after.bytes - before.bytes <= SLACK,
        "{name}: {} live bytes more after {rounds} more rounds",
        after.bytes - before.bytes
    );
    assert!(
        after.threads <= before.threads,
        "{name}: {:?} threads after {rounds} rounds, {:?} after {rounds} more",
        before.threads,
        after.threads
    );
    drop(rt);
}
