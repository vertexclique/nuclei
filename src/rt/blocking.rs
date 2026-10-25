//! The blocking pool: threads for work that blocks, so it never holds a
//! shard up.
//!
//! A job is a task whose future runs the closure in one poll; it goes into a
//! channel that idle pool threads wait on. Threads are started on demand up
//! to a maximum and leave after a keep-alive with nothing to do.
//!
// vertexia: the queue is unbounded, as the established pool's is: a cap in bytes needs a way to
// refuse a job, and `spawn_blocking` cannot fail (running it inline would block a shard). A
// fallible variant that refuses past a byte budget is the upgrade path.
//
//! No job is ever left without a thread. A submitter sends its job, then
//! looks at the idle count, and starts a thread when none is idle and there
//! is room. A thread that times out leaves the idle count, looks at the
//! channel again, then leaves the thread count and looks once more, taking
//! any job it finds and staying. Every one of these accesses is sequentially
//! consistent, so a submitter that saw the thread idle (or counted) has its
//! job seen by the thread's next look.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kovan_channel::flavors::RecvDeadline;

use super::task::{self, Runnable, Schedule, ScheduleInfo, Task};
use crate::sync::internal::{AtomicUsize, Ordering};

/// The pool's settings.
#[derive(Clone)]
pub(crate) struct PoolConfig {
    pub(crate) max_threads: usize,
    pub(crate) keep_alive: Duration,
    pub(crate) stack_size: Option<usize>,
    pub(crate) name: Arc<dyn Fn() -> String + Send + Sync>,
}

/// A runtime's blocking pool.
pub(crate) struct Pool {
    tx: kovan_channel::unbounded::Sender<Runnable>,
    rx: kovan_channel::unbounded::Receiver<Runnable>,
    threads: AtomicUsize,
    idle: AtomicUsize,
    config: PoolConfig,
}

/// The schedule of a blocking job: into the pool's channel.
struct PoolSched(Arc<Pool>);

// SAFETY: the push is the last use of `self`.
unsafe impl Schedule for PoolSched {
    fn schedule(&self, runnable: Runnable, _info: ScheduleInfo) {
        let pool = Arc::clone(&self.0);
        pool.submit(runnable);
    }
}

impl Pool {
    pub(crate) fn new(config: PoolConfig) -> Arc<Self> {
        let (tx, rx) = kovan_channel::unbounded();
        Arc::new(Self {
            tx,
            rx,
            threads: AtomicUsize::new(0),
            idle: AtomicUsize::new(0),
            config,
        })
    }

    /// Runs `f` on a pool thread.
    pub(crate) fn spawn<F, T>(self: &Arc<Self>, f: F) -> Task<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (runnable, task) = task::spawn(async move { f() }, PoolSched(Arc::clone(self)));
        runnable.schedule();
        task
    }

    /// Threads alive now.
    #[cfg(test)]
    pub(crate) fn threads(&self) -> usize {
        self.threads.load(Ordering::SeqCst)
    }

    fn submit(self: &Arc<Self>, runnable: Runnable) {
        self.tx.send(runnable);
        if self.idle.load(Ordering::SeqCst) == 0 {
            self.start_thread();
        }
    }

    /// Starts a thread if the pool has room for one.
    fn start_thread(self: &Arc<Self>) {
        let mut n = self.threads.load(Ordering::SeqCst);
        loop {
            if n >= self.config.max_threads {
                return;
            }
            match self
                .threads
                .compare_exchange_weak(n, n + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(actual) => n = actual,
            }
        }
        let pool = Arc::clone(self);
        let mut builder = std::thread::Builder::new().name((self.config.name)());
        if let Some(size) = self.config.stack_size {
            builder = builder.stack_size(size);
        }
        if let Err(err) = builder.spawn(move || pool.work()) {
            self.threads.fetch_sub(1, Ordering::SeqCst);
            self.cannot_start(err);
        }
    }

    /// A thread could not be started. With none alive, the queued jobs would
    /// wait forever: they run here instead, on the submitting thread.
    fn cannot_start(&self, _err: io::Error) {
        if self.threads.load(Ordering::SeqCst) == 0 {
            while let Some(runnable) = self.rx.try_recv() {
                runnable.run();
            }
        }
    }

    /// A pool thread's life.
    fn work(self: Arc<Self>) {
        let _alive = Alive::new();
        loop {
            self.idle.fetch_add(1, Ordering::SeqCst);
            let got = self
                .rx
                .recv_deadline(Instant::now() + self.config.keep_alive);
            self.idle.fetch_sub(1, Ordering::SeqCst);
            if let RecvDeadline::Msg(runnable) = got {
                runnable.run();
                continue;
            }
            // Idle for the keep-alive (or the pool is gone). A job sent while
            // this thread still counted as idle is its to run.
            if let Some(runnable) = self.rx.try_recv() {
                runnable.run();
                continue;
            }
            self.threads.fetch_sub(1, Ordering::SeqCst);
            // A job sent while this thread still counted toward the maximum
            // found no room for a new thread: it is this one's.
            if let Some(runnable) = self.rx.try_recv() {
                self.threads.fetch_add(1, Ordering::SeqCst);
                runnable.run();
                continue;
            }
            return;
        }
    }
}

/// Threads of every blocking pool alive now.
static ALIVE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Threads of the blocking pools alive now, for `stats`.
pub(crate) fn threads_alive() -> usize {
    ALIVE.load(core::sync::atomic::Ordering::Relaxed)
}

/// A pool thread, counted in [`ALIVE`] for its life, however it ends.
struct Alive;

impl Alive {
    fn new() -> Self {
        ALIVE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Self
    }
}

impl Drop for Alive {
    fn drop(&mut self) {
        ALIVE.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{Pool, PoolConfig};

    fn pool(max: usize, keep_alive: Duration) -> Arc<Pool> {
        Pool::new(PoolConfig {
            max_threads: max,
            keep_alive,
            stack_size: None,
            name: Arc::new(|| "test-blocking".to_string()),
        })
    }

    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut pause = Duration::from_micros(50);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(20));
        }
    }

    #[test]
    fn idle_threads_exit_after_the_keep_alive() {
        let pool = pool(4, Duration::from_millis(20));
        let tasks: Vec<_> = (0..4).map(|i| pool.spawn(move || i)).collect();
        for task in tasks {
            crate::sync::internal::block_on(task);
        }
        assert!(pool.threads() >= 1);
        wait_for("the idle threads to exit", || pool.threads() == 0);
        // A job after they left starts a thread again.
        assert_eq!(crate::sync::internal::block_on(pool.spawn(|| 9)), 9);
    }

    #[test]
    fn the_pool_never_runs_more_threads_than_its_maximum() {
        let pool = pool(2, Duration::from_secs(5));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tasks: Vec<_> = (0..32)
            .map(|_| {
                let (peak, live) = (Arc::clone(&peak), Arc::clone(&live));
                pool.spawn(move || {
                    use std::sync::atomic::Ordering::SeqCst;
                    let now = live.fetch_add(1, SeqCst) + 1;
                    peak.fetch_max(now, SeqCst);
                    std::thread::sleep(Duration::from_micros(300));
                    live.fetch_sub(1, SeqCst);
                })
            })
            .collect();
        for task in tasks {
            crate::sync::internal::block_on(task);
        }
        assert!(peak.load(std::sync::atomic::Ordering::SeqCst) <= 2);
        assert!(pool.threads() <= 2);
    }
}
