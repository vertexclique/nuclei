use core::future::{Future, poll_fn};
use core::pin::Pin;
use std::time::Duration;

use futures_io::AsyncWrite;

use super::{Command, Stdio};

/// A future's outcome within thirty seconds, or a failed test.
async fn within<F: Future>(fut: F) -> F::Output {
    crate::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("nothing came within thirty seconds")
}

async fn write_all(w: &mut (impl AsyncWrite + Unpin), mut buf: &[u8]) {
    while !buf.is_empty() {
        let n = poll_fn(|cx| Pin::new(&mut *w).poll_write(cx, buf))
            .await
            .unwrap();
        buf = &buf[n..];
    }
}

#[cfg(unix)]
mod unix {
    use std::os::unix::process::ExitStatusExt;
    use std::time::Instant;

    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    /// Whether `pid` (a child of ours) was reaped: no child of that pid is
    /// left to wait for.
    fn reaped(pid: u32) -> bool {
        // SAFETY: plain data, all zeroes valid; the kernel fills it.
        let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
        // SAFETY: a plain syscall that only looks (WNOWAIT).
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
    }

    /// Whether `pid` exited, reaped or not.
    fn exited(pid: u32) -> bool {
        // SAFETY: as above.
        let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
        // SAFETY: as above.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        r != 0 || crate::process::exit::status_of(&info).is_some()
    }

    #[test]
    fn exit_codes_come_back() {
        crate::block_on(async {
            assert_eq!(within(sh("exit 7").status()).await.unwrap().code(), Some(7));
            assert!(
                within(Command::new("true").status())
                    .await
                    .unwrap()
                    .success()
            );
            assert_eq!(
                within(Command::new("false").status()).await.unwrap().code(),
                Some(1)
            );
        });
    }

    #[test]
    fn a_killed_child_reports_its_signal() {
        crate::block_on(async {
            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            assert!(child.try_wait().unwrap().is_none(), "it runs");
            child.kill().unwrap();
            let status = within(child.wait()).await.unwrap();
            assert_eq!(status.signal(), Some(libc::SIGKILL));
            // A second wait gives it again; a kill of the exited child is
            // nothing to do.
            assert_eq!(within(child.wait()).await.unwrap(), status);
            child.kill().unwrap();
        });
    }

    #[test]
    fn output_collects_both_streams_until_the_exit() {
        crate::block_on(async {
            let output = within(sh("printf out; printf err >&2; exit 3").output())
                .await
                .unwrap();
            assert_eq!(output.status.code(), Some(3));
            assert_eq!(output.stdout, b"out");
            assert_eq!(output.stderr, b"err");
        });
    }

    #[test]
    fn stdin_reaches_the_child_and_its_end_ends_it() {
        crate::block_on(async {
            let mut child = Command::new("cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            write_all(&mut stdin, b"hello, child").await;
            drop(stdin);
            let output = within(child.wait_with_output()).await.unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, b"hello, child");
        });
    }

    /// A child waited for in a worker's task.
    #[test]
    fn a_child_is_waited_for_on_a_worker() {
        let rt = crate::runtime::Runtime::builder()
            .worker_threads(2)
            .pin_workers(false)
            .build()
            .unwrap();
        let task = rt.spawn(async { within(sh("exit 9").status()).await });
        assert_eq!(rt.block_on(task).unwrap().code(), Some(9));
    }

    /// A thousand children live at once: each one's exit comes, half of
    /// them killed, half exiting on their own. Alone in its process: the
    /// thousand pidfds and pids would crowd tests that count descriptors.
    #[test]
    fn a_thousand_live_children_each_report_their_exit() {
        if !crate::alone::alone(
            "process::tests::unix::a_thousand_live_children_each_report_their_exit",
        ) {
            return;
        }
        crate::block_on(async {
            let mut children: Vec<_> = (0..1000)
                .map(|_| Command::new("sleep").arg("2").spawn().unwrap())
                .collect();
            for child in children.iter_mut().step_by(2) {
                child.kill().unwrap();
            }
            for (i, child) in children.iter_mut().enumerate() {
                let status = within(child.wait()).await.unwrap();
                if i % 2 == 0 {
                    assert_eq!(status.signal(), Some(libc::SIGKILL), "child {i}");
                } else {
                    assert!(status.success(), "child {i}: {status:?}");
                }
            }
        });
    }

    /// A child whose `Child` went before it exited is reaped all the same.
    #[test]
    fn a_child_whose_child_went_is_reaped() {
        crate::block_on(async {
            let child = sh("sleep 0.1").spawn().unwrap();
            let pid = child.id();
            drop(child);
            let deadline = Instant::now() + Duration::from_secs(30);
            while !reaped(pid) {
                assert!(Instant::now() < deadline, "the child was never reaped");
                crate::time::sleep(Duration::from_millis(5)).await;
            }
        });
    }

    /// A child that exited before anything watched it (a zombie when its
    /// watch is made; kqueue then has no process to watch) is reaped.
    #[test]
    fn a_child_that_exited_before_its_watch_is_reaped() {
        crate::block_on(async {
            let mut child = Command::new("true").spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !exited(child.id()) {
                assert!(Instant::now() < deadline, "the child never exited");
                std::thread::yield_now();
            }
            assert!(within(child.wait()).await.unwrap().success());
            assert!(reaped(child.id()));
        });
    }

    /// Children Linux gave no pidfd are reaped on `SIGCHLD`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn the_sigchld_reaper_takes_children_without_a_pidfd() {
        crate::process::exit::NO_PIDFD.with(|no| no.set(true));
        crate::block_on(async {
            assert_eq!(within(sh("exit 5").status()).await.unwrap().code(), Some(5));
            let mut child = Command::new("sleep").arg("30").spawn().unwrap();
            child.kill().unwrap();
            assert_eq!(
                within(child.wait()).await.unwrap().signal(),
                Some(libc::SIGKILL)
            );
        });
        crate::process::exit::NO_PIDFD.with(|no| no.set(false));
    }

    /// A ring without `WAITID` polls the pidfd and reaps the child itself.
    #[cfg(ring)]
    #[test]
    fn a_ring_without_waitid_polls_the_pidfd() {
        crate::rt::shard::ring::exits::NO_WAITID.with(|no| no.set(true));
        crate::block_on(async {
            assert_eq!(within(sh("exit 4").status()).await.unwrap().code(), Some(4));
        });
        crate::rt::shard::ring::exits::NO_WAITID.with(|no| no.set(false));
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    fn cmd(script: &str) -> Command {
        let mut command = Command::new("cmd");
        command.args(["/C", script]);
        command
    }

    #[test]
    fn exit_codes_come_back() {
        crate::block_on(async {
            assert_eq!(
                within(cmd("exit 7").status()).await.unwrap().code(),
                Some(7)
            );
            assert!(within(cmd("exit 0").status()).await.unwrap().success());
        });
    }

    #[test]
    fn a_killed_child_reports_its_end() {
        crate::block_on(async {
            let mut child = Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            assert!(child.try_wait().unwrap().is_none(), "it runs");
            child.kill().unwrap();
            let status = within(child.wait()).await.unwrap();
            assert_eq!(status.code(), Some(1));
            child.kill().unwrap();
        });
    }

    #[test]
    fn output_collects_both_streams_until_the_exit() {
        crate::block_on(async {
            let output = within(cmd("(echo out)& (echo err)1>&2& exit /B 3").output())
                .await
                .unwrap();
            assert_eq!(output.status.code(), Some(3));
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "out");
            assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "err");
        });
    }

    #[test]
    fn stdin_reaches_the_child_and_its_end_ends_it() {
        crate::block_on(async {
            let mut child = Command::new("sort")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            write_all(&mut stdin, b"b\r\na\r\n").await;
            drop(stdin);
            let output = within(child.wait_with_output()).await.unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout), "a\r\nb\r\n");
        });
    }

    #[test]
    fn many_children_each_report_their_exit() {
        crate::block_on(async {
            let mut children: Vec<_> = (0..100)
                .map(|i| cmd(&format!("exit {i}")).spawn().unwrap())
                .collect();
            for (i, child) in children.iter_mut().enumerate() {
                assert_eq!(within(child.wait()).await.unwrap().code(), Some(i as i32));
            }
        });
    }
}
