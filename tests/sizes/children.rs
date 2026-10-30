//! A child's output read off its pipe, and this binary's standard streams in a child.

use super::*;

/// A child's output of every size, read off its pipe.
#[cfg(unix)]
#[test]
fn process_output() {
    let dir = temp_dir();
    let path = dir.join("out");
    for &n in sizes() {
        let data = pattern(n);
        std::fs::write(&path, data).unwrap();
        let out = within(
            format!("cat of {n}"),
            nuclei::process::Command::new("cat").arg(&path).output(),
        )
        .unwrap();
        assert!(out.status.success());
        assert!(
            out.stdout == data,
            "cat of {n}: {} bytes came, not the ones in the file",
            out.stdout.len()
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// Set in a process of this test binary that echoes its stdin.
const ECHO: &str = "NUCLEI_SIZES_ECHO";

/// The child of [`stdin_and_stderr`]: everything on stdin, read to its end, goes to stderr.
#[test]
fn echo_child() {
    if std::env::var_os(ECHO).is_none() {
        return;
    }
    within("the echo", async {
        let mut got = Vec::new();
        nuclei::io::stdin().read_to_end(&mut got).await.unwrap();
        let mut err = nuclei::io::stderr();
        err.write_all(&got).await.unwrap();
        err.flush().await.unwrap();
    });
}

/// This thread's standard streams at every size: what a child reads to the end of its stdin it
/// writes to its stderr whole.
#[cfg(unix)]
#[test]
fn stdin_and_stderr() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    for &n in sizes() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "children::echo_child", "--test-threads=1"])
            .env(ECHO, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || stdin.write_all(pattern(n)).unwrap());
        let mut err = child.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            err.read_to_end(&mut got).unwrap();
            got
        });
        let status = wait_until(&mut child, Instant::now() + DEADLINE);
        writer.join().unwrap();
        let got = reader.join().unwrap();
        assert!(
            status.success(),
            "the echo of {n} failed: {}",
            String::from_utf8_lossy(&got[..got.len().min(4096)])
        );
        assert!(
            got == pattern(n),
            "the echo of {n}: {} bytes came, not the ones sent",
            got.len()
        );
    }
}
