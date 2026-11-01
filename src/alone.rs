//! Running one test alone in a process of its own: for a case that needs
//! the whole process (the next descriptor number, a seccomp filter, a
//! resource limit that cannot be raised again); and a watchdog that ends the
//! process of a test that hangs.

use std::io::Read;
#[cfg(unix)]
use std::io::Write;
use std::time::Duration;

/// Set in a process of this test binary that runs one test alone.
const ALONE: &str = "NUCLEI_TEST_ALONE";

/// Runs the test `name` again, alone in a new process of this test binary,
/// and checks that it ran and passed there. Returns true in that process,
/// where the caller then runs its case.
pub(crate) fn alone(name: &str) -> bool {
    if std::env::var_os(ALONE).is_some() {
        return true;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1"])
        .env(ALONE, "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut backoff = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("{name} did not finish in its own process");
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(100));
    };
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(status.success(), "{name} failed in its own process:\n{out}");
    assert!(
        out.contains("test result: ok. 1 passed"),
        "{name} did not run in its own process:\n{out}"
    );
    false
}

/// The descriptors this process has open. Listed from the directory the
/// system keeps for it (Linux, macOS); elsewhere (FreeBSD's `/dev/fd` is
/// only there with `fdescfs`, and then lists but three) each number up to the
/// soft limit is asked of the kernel.
#[cfg(unix)]
pub(crate) fn open_descriptors() -> Vec<i32> {
    #[cfg(target_os = "linux")]
    const LISTING: Option<&str> = Some("/proc/self/fd");
    #[cfg(target_os = "macos")]
    const LISTING: Option<&str> = Some("/dev/fd");
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const LISTING: Option<&str> = None;
    if let Some(dir) = LISTING {
        let listed = std::fs::read_dir(dir).map(|entries| {
            entries
                .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
                .collect::<Vec<_>>()
        });
        if let Ok(fds) = listed {
            return fds;
        }
    }
    // SAFETY: plain data, all zeroes valid; the kernel fills it.
    let mut limit: libc::rlimit = unsafe { core::mem::zeroed() };
    // SAFETY: a plain call with a valid struct.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur.min(1 << 20) as i32
    } else {
        1024
    };
    // SAFETY: F_GETFD only reads the descriptor's flags.
    (0..soft)
        .filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0)
        .collect()
}

/// A test's watchdog: unless dropped within `limit`, it writes `what` and
/// `report()` to the process's standard error (past the harness's capture,
/// which an abort throws away) and aborts, so a hang fails at once and says
/// where it was rather than holding the suite until the job's own limit.
#[cfg(unix)]
pub(crate) struct Watchdog(Option<std::sync::mpsc::Sender<()>>);

#[cfg(unix)]
impl Watchdog {
    pub(crate) fn new(
        what: &'static str,
        limit: Duration,
        report: impl Fn() -> String + Send + 'static,
    ) -> Self {
        let (done, wait) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = wait.recv_timeout(limit) {
                let _ = writeln!(std::io::stderr(), "{what} hung: {}", report());
                std::process::abort();
            }
        });
        Self(Some(done))
    }
}

#[cfg(unix)]
impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            let _ = done.send(());
        }
    }
}
