//! Child processes: a [`Command`](crate::process::Command) spawns one, and
//! its [`Child`](crate::process::Child) waits for the exit without holding a
//! thread and talks to it through async pipes.
//!
//! A child's exit is reported by a shard's driver, which reaps the child
//! (see `exit`); a wait never blocks a thread. A child whose `Child` goes
//! before it exits is still reaped (on unix), by the shard that watches it,
//! so it leaves no zombie behind while a runtime runs.

use core::future::poll_fn;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::Arc;

use futures_io::AsyncRead;

pub use std::process::{ExitStatus, Output, Stdio};

pub(crate) mod exit;
#[cfg(any(target_os = "linux", target_os = "android"))]
mod fallback;
mod stdio;
#[cfg(test)]
mod tests;

pub use stdio::{ChildStderr, ChildStdin, ChildStdout};

use exit::Exit;

/// Reaps the children of the `SIGCHLD` reaper that exited: the signal
/// module's dispatch, on a delivery of `SIGCHLD`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn reap_fallback() {
    fallback::reap();
}

/// A process to spawn, as [`std::process::Command`] describes it.
#[derive(Debug)]
pub struct Command {
    std: std::process::Command,
    /// What `output` gives the child's stdin unless this says otherwise.
    stdin_set: bool,
    kill_on_drop: bool,
}

impl Command {
    /// A command for `program`, with its environment and working directory
    /// the process's, and its standard streams inherited.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            std: std::process::Command::new(program),
            stdin_set: false,
            kill_on_drop: false,
        }
    }

    /// Adds an argument.
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.std.arg(arg);
        self
    }

    /// Adds arguments.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.std.args(args);
        self
    }

    /// Sets an environment variable of the child.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.std.env(key, value);
        self
    }

    /// Sets environment variables of the child.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.std.envs(vars);
        self
    }

    /// Takes an environment variable out of the child's environment.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.std.env_remove(key);
        self
    }

    /// Starts the child with no environment but what is set after.
    pub fn env_clear(&mut self) -> &mut Self {
        self.std.env_clear();
        self
    }

    /// Sets the child's working directory.
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.std.current_dir(dir);
        self
    }

    /// Sets the child's stdin.
    pub fn stdin(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.std.stdin(cfg);
        self.stdin_set = true;
        self
    }

    /// Sets the child's stdout.
    pub fn stdout(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.std.stdout(cfg);
        self
    }

    /// Sets the child's stderr.
    pub fn stderr(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.std.stderr(cfg);
        self
    }

    /// Whether the child is ended, as [`Child::start_kill`] ends it, when
    /// its [`Child`] is dropped before it exits (default false: it runs on,
    /// and is still reaped once it exits).
    ///
    /// ```no_run
    /// # nuclei::block_on(async {
    /// use nuclei::process::Command;
    ///
    /// let child = Command::new("sleep").arg("60").kill_on_drop(true).spawn()?;
    /// drop(child); // the child is ended here
    /// # std::io::Result::Ok(())
    /// # }).unwrap();
    /// ```
    pub fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.kill_on_drop = kill;
        self
    }

    /// Whether [`kill_on_drop`](Self::kill_on_drop) is set.
    pub fn get_kill_on_drop(&self) -> bool {
        self.kill_on_drop
    }

    /// The std command underneath, for what this one does not mirror.
    pub fn as_std(&self) -> &std::process::Command {
        &self.std
    }

    /// The std command underneath, to change.
    pub fn as_std_mut(&mut self) -> &mut std::process::Command {
        &mut self.std
    }

    /// The std command underneath, taken.
    pub fn into_std(self) -> std::process::Command {
        self.std
    }

    /// Spawns the child. Its piped streams are async.
    ///
    /// # Errors
    ///
    /// When the child cannot start, or its pipes cannot be taken over.
    pub fn spawn(&mut self) -> io::Result<Child> {
        self.spawn_with(std::process::Command::spawn)
    }

    /// Spawns the child through `with`, which starts it from the std
    /// command underneath: for a way to start a child this type does not
    /// mirror, such as a platform's own spawn call with extra attributes.
    /// The child `with` returns is then this one's, its piped streams async.
    ///
    /// ```no_run
    /// # nuclei::block_on(async {
    /// use nuclei::process::Command;
    ///
    /// let mut child = Command::new("true").spawn_with(|std| std.spawn())?;
    /// assert!(child.wait().await?.success());
    /// # std::io::Result::Ok(())
    /// # }).unwrap();
    /// ```
    ///
    /// # Errors
    ///
    /// What `with` returns, and when the child's pipes cannot be taken
    /// over.
    pub fn spawn_with(
        &mut self,
        with: impl FnOnce(&mut std::process::Command) -> io::Result<std::process::Child>,
    ) -> io::Result<Child> {
        let mut std = with(&mut self.std)?;
        let exit = match Exit::new(&std) {
            Ok(exit) => exit,
            Err(e) => {
                if self.kill_on_drop {
                    // The error that matters is the one returned.
                    let _ = std.kill();
                }
                return Err(e);
            }
        };
        let (stdin, stdout, stderr) = (std.stdin.take(), std.stdout.take(), std.stderr.take());
        // Made before the pipes are taken over: should one fail, its drop
        // still reaps the child (and ends it, if asked).
        let mut child = Child {
            stdin: None,
            stdout: None,
            stderr: None,
            exit,
            kill_on_drop: self.kill_on_drop,
            _std: std,
        };
        child.stdin = stdin.map(ChildStdin::new).transpose()?;
        child.stdout = stdout.map(ChildStdout::new).transpose()?;
        child.stderr = stderr.map(ChildStderr::new).transpose()?;
        Ok(child)
    }

    /// Spawns the child and waits for its exit; its streams are inherited
    /// unless set.
    ///
    /// # Errors
    ///
    /// As for [`spawn`](Self::spawn) and [`Child::wait`].
    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        self.spawn()?.wait().await
    }

    /// Spawns the child with its stdout and stderr piped (and its stdin
    /// empty, unless set), and collects both until its exit.
    ///
    /// # Errors
    ///
    /// As for [`spawn`](Self::spawn) and [`Child::wait_with_output`].
    pub async fn output(&mut self) -> io::Result<Output> {
        if !self.stdin_set {
            self.std.stdin(Stdio::null());
        }
        self.std.stdout(Stdio::piped()).stderr(Stdio::piped());
        self.spawn()?.wait_with_output().await
    }
}

impl From<std::process::Command> for Command {
    fn from(std: std::process::Command) -> Self {
        Self {
            std,
            stdin_set: false,
            kill_on_drop: false,
        }
    }
}

/// A spawned child process.
#[derive(Debug)]
pub struct Child {
    /// The child's stdin, if piped.
    pub stdin: Option<ChildStdin>,
    /// The child's stdout, if piped.
    pub stdout: Option<ChildStdout>,
    /// The child's stderr, if piped.
    pub stderr: Option<ChildStderr>,
    exit: Arc<Exit>,
    kill_on_drop: bool,
    /// The std child, kept for what it owns (its process handle).
    _std: std::process::Child,
}

impl core::fmt::Debug for Exit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Exit")
            .field("pid", &self.pid())
            .finish_non_exhaustive()
    }
}

impl Child {
    /// The child's process id.
    pub fn id(&self) -> u32 {
        self.exit.pid()
    }

    /// Waits for the child's exit. Its stdin, if piped, is closed first, so
    /// a child that reads it to its end can exit.
    ///
    /// # Errors
    ///
    /// When the exit cannot be watched, or the child was reaped by
    /// something else (`ECHILD`: `SIGCHLD` ignored, or a `waitpid` of the
    /// program's own).
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        poll_fn(|cx| self.exit.poll_wait(cx)).await
    }

    /// The child's status if it exited, else `None`, without waiting.
    ///
    /// # Errors
    ///
    /// When the child's state cannot be read.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.exit.try_wait()
    }

    /// Starts ending the child, without waiting for it to go: `SIGKILL` on
    /// unix, `TerminateProcess` on Windows. A child that exited already is
    /// left alone.
    ///
    /// # Errors
    ///
    /// When the signal or the call fails.
    pub fn start_kill(&mut self) -> io::Result<()> {
        self.exit.kill()
    }

    /// Ends the child, as [`start_kill`](Self::start_kill) does, and waits
    /// for its exit.
    ///
    /// ```no_run
    /// # nuclei::block_on(async {
    /// use nuclei::process::Command;
    ///
    /// let mut child = Command::new("sleep").arg("60").spawn()?;
    /// child.kill().await?;
    /// assert!(child.try_wait()?.is_some());
    /// # std::io::Result::Ok(())
    /// # }).unwrap();
    /// ```
    ///
    /// # Errors
    ///
    /// As for [`start_kill`](Self::start_kill) and [`wait`](Self::wait).
    pub async fn kill(&mut self) -> io::Result<()> {
        self.start_kill()?;
        self.wait().await.map(drop)
    }

    /// Waits for the child's exit, collecting its stdout and stderr (those
    /// piped) until then. Its stdin, if piped, is closed first.
    ///
    /// # Errors
    ///
    /// As for [`wait`](Self::wait), and when a pipe cannot be read.
    pub async fn wait_with_output(mut self) -> io::Result<Output> {
        drop(self.stdin.take());
        let (mut stdout, mut stderr) = (self.stdout.take(), self.stderr.take());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        poll_fn(|cx| {
            let out_done = drain(&mut stdout, &mut out, cx)?.is_ready();
            let err_done = drain(&mut stderr, &mut err, cx)?.is_ready();
            if out_done && err_done {
                Poll::Ready(Ok::<(), io::Error>(()))
            } else {
                Poll::Pending
            }
        })
        .await?;
        let status = self.wait().await?;
        Ok(Output {
            status,
            stdout: out,
            stderr: err,
        })
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.kill_on_drop {
            // A drop cannot report a failure, and the kill of a child of
            // ours fails only once it is gone.
            let _ = self.exit.kill();
        }
        // A child that runs on is still reaped once it exits.
        #[cfg(unix)]
        self.exit.watch_orphan();
    }
}

/// Reads `pipe` into `into` until its end (then lets it go) or until it
/// would wait.
fn drain<R: AsyncRead + Unpin>(
    pipe: &mut Option<R>,
    into: &mut Vec<u8>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    const STEP: usize = 16 * 1024;
    while let Some(reader) = pipe.as_mut() {
        let at = into.len();
        into.resize(at + STEP, 0);
        let read = Pin::new(reader).poll_read(cx, &mut into[at..]);
        match read {
            Poll::Ready(Ok(0)) => {
                into.truncate(at);
                *pipe = None;
            }
            Poll::Ready(Ok(n)) => into.truncate(at + n),
            Poll::Ready(Err(err)) => {
                into.truncate(at);
                return Poll::Ready(Err(err));
            }
            Poll::Pending => {
                into.truncate(at);
                return Poll::Pending;
            }
        }
    }
    Poll::Ready(Ok(()))
}
