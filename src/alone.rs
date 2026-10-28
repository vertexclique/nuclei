//! Running one test alone in a process of its own: for a case that needs
//! the whole process (the next descriptor number, a seccomp filter, a
//! resource limit that cannot be raised again).

use std::io::Read;
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
