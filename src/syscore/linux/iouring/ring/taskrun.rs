//! Whether the kernel keeps `IORING_SQ_TASKRUN` for a ring that defers its task work: the flag a
//! look at the completions trusts to know whether an enter would bring any. Kernels differ here,
//! so it is found once per process by trying it, as `files::release_at_once` is.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;

use super::{Ring, RingConfig, Sqe};

/// Whether a single-issuer ring with `IORING_SETUP_TASKRUN_FLAG` is accepted and flags the task
/// work that a read of an eventfd (the wake's own operation) leaves when the eventfd is written.
/// Anything that goes wrong in the trial says no, which costs only the enters of the looks.
pub(crate) fn flag_works() -> bool {
    static WORKS: OnceLock<bool> = OnceLock::new();
    *WORKS.get_or_init(|| trial().unwrap_or(false))
}

fn trial() -> std::io::Result<bool> {
    // SAFETY: a plain syscall; the descriptor it returns is owned at once.
    let wake = unsafe {
        let fd = libc::eventfd(0, libc::EFD_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        OwnedFd::from_raw_fd(fd)
    };
    let mut ring = Ring::new(&RingConfig {
        taskrun_flag: true,
        ..RingConfig::new(4)
    })?;
    let mut buf = Box::new(0u64);
    let read = Sqe::read(
        wake.as_raw_fd(),
        core::ptr::from_mut(&mut *buf).cast(),
        8,
        0,
    )
    .user_data(1);
    ring.push(&read)
        .map_err(|_| std::io::Error::other("no room in a new ring"))?;
    ring.submit()?;
    if ring.taskrun_pending() {
        return Ok(false);
    }
    // SAFETY: writes eight bytes of our own to our own eventfd.
    unsafe { libc::write(wake.as_raw_fd(), 1u64.to_ne_bytes().as_ptr().cast(), 8) };
    // The write queues the read's completion as task work in the call itself; the wait is for a
    // kernel that does it later.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
    let flagged = loop {
        if ring.taskrun_pending() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::yield_now();
    };
    // The read ends before the ring and its buffer go.
    ring.submit_and_wait(1, Some(std::time::Duration::from_millis(100)))?;
    let done = ring.completions().any(|cqe| cqe.user_data == 1);
    Ok(flagged && done)
}
