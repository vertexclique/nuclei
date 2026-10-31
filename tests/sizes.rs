//! Every size that picks a path inside nuclei, through every call that takes one: 0, 1, and one
//! below, at and one above each threshold in [`THRESHOLDS`], on the backend the process runs (CI
//! runs the suite on io_uring and again with `NUCLEI_BACKEND=epoll`). Each case checks the bytes
//! exactly and ends within [`DEADLINE`]. A datagram the kernel refuses must fail as std's same
//! call does. On io_uring the receive group's buffer size is swept too, each size in a process of
//! its own, since the configuration is the process's.

use std::future::Future;
use std::io::{self, IoSlice};
use std::sync::OnceLock;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use nuclei::fs::{File, OpenOptions};
use nuclei::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use nuclei::io::{BufReader, BufWriter, Handle};
use nuclei::net::{TcpStream, UdpSocket};
#[cfg(unix)]
use nuclei::net::{UnixDatagram, UnixStream};

/// The longest one case may take on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(60);

/// A size constant of nuclei, held to its declaration in `file`.
struct Threshold {
    file: &'static str,
    source: &'static str,
    declared: &'static str,
    values: &'static [usize],
}

macro_rules! threshold {
    ($file:literal, $declared:literal, $($value:expr),+) => {
        Threshold {
            file: $file,
            source: include_str!(concat!("../", $file)),
            declared: $declared,
            values: &[$($value),+],
        }
    };
}

/// Every size constant that picks a path, mirrored; `the_thresholds_are_the_codes` fails when
/// one of them changes.
const THRESHOLDS: &[Threshold] = &[
    threshold!(
        "src/buf/mod.rs",
        "pub(crate) const CLASSES: [usize; 4] = [512, 4 * 1024, 16 * 1024, 64 * 1024];",
        512,
        4 << 10,
        16 << 10,
        64 << 10
    ),
    threshold!(
        "src/io/source/direct.rs",
        "pub(super) const READ_READINESS: usize = 16 << 10;",
        16 << 10
    ),
    threshold!(
        "src/io/source/direct.rs",
        "const WRITE_READINESS: usize = 0;",
        0
    ),
    threshold!(
        "src/io/source/direct.rs",
        "const READ_RING: usize = 16 << 10;",
        16 << 10
    ),
    threshold!("src/io/source/direct.rs", "const WRITE_RING: usize = 0;", 0),
    threshold!(
        "src/io/source.rs",
        "pub(crate) const OWNED_DATAGRAM_MIN: usize = 12 * 1024;",
        12 << 10
    ),
    threshold!(
        "src/io/source/write.rs",
        "const BEHIND_MIN: usize = 16 * 1024;",
        16 << 10
    ),
    threshold!(
        "src/fs/file.rs",
        "pub(crate) const READ_MIN: usize = 64 * 1024;",
        64 << 10
    ),
    threshold!(
        "src/fs/file.rs",
        "const WRITE_MIN: usize = 64 * 1024;",
        64 << 10
    ),
    threshold!(
        "src/io/source/ring.rs",
        "pub(crate) const SEND_ZC_MIN: usize = 1 << 20;",
        1 << 20
    ),
    threshold!(
        "src/io/source/ring.rs",
        "const SEND_ZC_CHUNK: usize = 1 << 20;",
        1 << 20
    ),
    threshold!(
        "src/net/send_file.rs",
        "const SEND_FILE_CHUNK: u64 = 1 << 20;",
        1 << 20
    ),
    threshold!(
        "src/io/util/copy.rs",
        "const DEFAULT_BUF_SIZE: usize = 8 * 1024;",
        8 << 10
    ),
    threshold!(
        "src/io/util/buffered.rs",
        "const DEFAULT_CAPACITY: usize = 8 * 1024;",
        8 << 10
    ),
    threshold!("src/io/util/read_ext.rs", "const MIN_READ: usize = 32;", 32),
    threshold!(
        "src/io/util/read_ext.rs",
        "const MAX_READ: usize = 64 * 1024;",
        64 << 10
    ),
    threshold!(
        "src/io/util/stdio.rs",
        "const MAX_BUF: usize = 16 * 1024;",
        16 << 10
    ),
    threshold!(
        "src/process/mod.rs",
        "    const STEP: usize = 16 * 1024;",
        16 << 10
    ),
    threshold!("src/rt/shard/recv.rs", "const MIN_CHARGE: u32 = 256;", 256),
    // What one receive of the default group (256 buffers of 16 KiB) may hold.
    threshold!(
        "src/rt/shard/recv.rs",
        "const CAP_SHARE: usize = 8;",
        (256 << 14) / 8
    ),
    threshold!(
        "src/config.rs",
        "const DEFAULT_RECV_BUFFERS: u16 = 256;",
        256 << 14
    ),
    threshold!(
        "src/config.rs",
        "const DEFAULT_RECV_BUFFER_SIZE: u32 = 16 * 1024;",
        16 << 10
    ),
];

/// The pool's cap, beyond which no buffer of the pool holds a datagram.
const LARGE: Threshold = threshold!(
    "src/buf/mod.rs",
    "pub(crate) const DEFAULT_CAP: usize = 64 << 20;",
    64 << 20
);

/// The most slices one vectored call hands the kernel.
#[cfg(unix)]
const IOV_MAX: Threshold = threshold!("src/io/sys/unix.rs", "const IOV_MAX: usize = 1024;", 1024);

/// The room a multishot datagram receive needs in a group buffer: the kernel's 16-byte header, the
/// sender and the largest datagram.
#[cfg(target_os = "linux")]
const DATAGRAM_ROOM: Threshold = threshold!(
    "src/io/source/ring.rs",
    "pub(crate) const DATAGRAM_ROOM: u32 = (size_of::<
    crate::syscore::linux::iouring::ring::IoUringRecvmsgOut,
>() + size_of::<libc::sockaddr_storage>()
    + u16::MAX as usize) as u32;",
    16 + size_of::<libc::sockaddr_storage>() + u16::MAX as usize
);

#[test]
fn the_thresholds_are_the_codes() {
    let all = THRESHOLDS.iter().chain([&LARGE]);
    #[cfg(unix)]
    let all = all.chain([&IOV_MAX]);
    #[cfg(target_os = "linux")]
    let all = all.chain([&DATAGRAM_ROOM]);
    for t in all {
        assert!(
            t.source.contains(t.declared),
            "{} no longer declares `{}` (the sweep mirrors {:?}): update tests/sizes.rs",
            t.file,
            t.declared,
            t.values
        );
    }
}

/// 0, 1, one below, at and one above every threshold, and the pool's cap.
fn sizes() -> &'static [usize] {
    static SIZES: OnceLock<Vec<usize>> = OnceLock::new();
    SIZES.get_or_init(|| {
        let mut sizes = vec![0, 1];
        for &v in THRESHOLDS.iter().chain([&LARGE]).flat_map(|t| t.values) {
            sizes.extend([v.saturating_sub(1), v, v + 1]);
        }
        sizes.sort_unstable();
        sizes.dedup();
        sizes
    })
}

/// The bytes every case sends, a prefix of them: no two nearby positions alike, so a byte out of
/// place shows.
fn pattern(len: usize) -> &'static [u8] {
    static PATTERN: OnceLock<Vec<u8>> = OnceLock::new();
    let all = PATTERN.get_or_init(|| {
        (0..(LARGE.values[0] + 4096) as u64)
            .map(|i| (i.wrapping_mul(0x9E37_79B9) >> 13 ^ i >> 8) as u8)
            .collect()
    });
    &all[..len]
}

/// Runs `case` on this thread's shard, failing the test with `what` past [`DEADLINE`].
fn within<T>(what: impl std::fmt::Display, case: impl Future<Output = T>) -> T {
    nuclei::block_on(async {
        nuclei::time::timeout(DEADLINE, case)
            .await
            .unwrap_or_else(|_| panic!("{what}: not done within {DEADLINE:?}"))
    })
}

/// Waits for `child` until `deadline`, backing off from 1 ms to 100 ms between looks.
#[cfg(unix)]
fn wait_until(child: &mut std::process::Child, deadline: Instant) -> std::process::ExitStatus {
    let mut pause = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("a child process did not finish in time");
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(100));
    }
}

/// Where a row starts in a file past its start: one byte past a page.
const OFFSET: u64 = 4097;

/// A new directory under the system's temporary one, for this process.
fn temp_dir() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nuclei-sizes-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[path = "sizes/children.rs"]
mod children;
#[path = "sizes/datagrams.rs"]
mod datagrams;
#[path = "sizes/files.rs"]
mod files;
#[cfg(target_os = "linux")]
#[path = "sizes/groups.rs"]
mod groups;
#[path = "sizes/streams.rs"]
mod streams;
