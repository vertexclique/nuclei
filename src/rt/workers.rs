//! A runtime's worker shards: the slots they fill, starting and stopping
//! one, and where `spawn` places a task.
//!
//! A runtime has room for a set number of workers over its life, fixed when
//! it is built (four times the workers it starts with unless set
//! otherwise). Slots fill in order as workers start, and a slot once filled
//! keeps its shard, so placement reads them without a lock. A worker asked
//! to stop takes no new task from then on, runs the ones it has to their
//! end, and exits; its slot is not filled again.

use core::cell::Cell;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use super::context;
use super::runtime::Inner;
use super::shard::{Limits, Shard, Shared};
use crate::sync::internal::{AtomicUsize, Ordering};

thread_local! {
    /// The runtime and the shard the spawns of this thread (one that is not
    /// that runtime's worker) went to last.
    static HOME: Cell<Option<(u64, usize)>> = const { Cell::new(None) };
}

/// How a worker is started.
pub(crate) struct Start {
    pub(crate) limits: Limits,
    pub(crate) stack: usize,
    pub(crate) name: Arc<dyn Fn() -> String + Send + Sync>,
    /// The cores workers are pinned to, in turn; empty for none.
    pub(crate) cores: Vec<usize>,
}

/// A runtime's worker shards.
pub(crate) struct Workers {
    slots: Box<[OnceLock<Arc<Shared>>]>,
    /// Slots claimed by a starting worker: `[0, claimed)`; a claimed slot is
    /// empty until its shard is made.
    claimed: AtomicUsize,
    /// Workers taking tasks: started and not stopping.
    running: AtomicUsize,
    /// The fewest workers a stop leaves running.
    min: usize,
    start: Start,
    threads: Mutex<Vec<JoinHandle<()>>>,
    exits: kovan_channel::unbounded::Sender<()>,
}

impl Workers {
    pub(crate) fn new(
        capacity: usize,
        min: usize,
        start: Start,
        exits: kovan_channel::unbounded::Sender<()>,
    ) -> Self {
        Self {
            slots: (0..capacity).map(|_| OnceLock::new()).collect(),
            claimed: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            min,
            start,
            threads: Mutex::new(Vec::new()),
            exits,
        }
    }

    /// Workers taking tasks.
    pub(crate) fn running(&self) -> usize {
        self.running.load(Ordering::Relaxed)
    }

    /// The shards of every slot filled so far.
    pub(crate) fn shards(&self) -> impl Iterator<Item = &Arc<Shared>> {
        let claimed = self.claimed.load(Ordering::Acquire).min(self.slots.len());
        self.slots[..claimed].iter().filter_map(OnceLock::get)
    }

    /// Picks the shard a new task goes to, or `None` when no worker takes
    /// tasks. Spawned on one of the runtime's workers: that worker, unless it
    /// holds more than `margin` live tasks beyond a shard picked at random,
    /// which then takes it. Spawned on any other thread: that thread's home
    /// shard (where its spawns went last), by the same rule, the shard that
    /// takes it becoming the new home; a first spawn takes the less loaded
    /// of two distinct shards picked at random. Tasks spawned together so
    /// stay on one shard, and no shard holds more than `margin` live tasks
    /// beyond another for long.
    pub(crate) fn place(&self, runtime: u64, margin: usize) -> Option<&Arc<Shared>> {
        let n = self.claimed.load(Ordering::Acquire).min(self.slots.len());
        let take = |i: usize| self.slots[i].get().filter(|s| s.takes_tasks());
        match n {
            0 => return None,
            1 => return take(0),
            _ => {}
        }
        let load = |s: &Shared| s.load.load(Ordering::Relaxed);
        // Stays on `home` unless a shard picked at random is lighter by more
        // than the margin.
        let stay_or_move = |home: usize| {
            let mine = take(home)?;
            let other = (home + 1 + random_below(n - 1)) % n;
            Some(match take(other) {
                Some(o) if load(mine) > load(o).saturating_add(margin) => (other, o),
                _ => (home, mine),
            })
        };
        let local = context::current(|current| current.and_then(|s| s.worker_index(runtime)));
        if let Some((_, shard)) = local.filter(|&i| i < n).and_then(stay_or_move) {
            return Some(shard);
        }
        let home = HOME
            .try_with(Cell::get)
            .ok()
            .flatten()
            .filter(|&(owner, i)| owner == runtime && i < n)
            .and_then(|(_, i)| stay_or_move(i));
        let (index, shard) = match home {
            Some(chosen) => chosen,
            None => {
                let a = random_below(n);
                let b = (a + 1 + random_below(n - 1)) % n;
                match (take(a), take(b)) {
                    (Some(x), Some(y)) if load(y) < load(x) => (b, y),
                    (Some(x), _) => (a, x),
                    (None, Some(y)) => (b, y),
                    // Both stopping: any worker that takes tasks.
                    (None, None) => (0..n).find_map(|i| take(i).map(|s| (i, s)))?,
                }
            }
        };
        let _ = HOME.try_with(|h| h.set(Some((runtime, index))));
        Some(shard)
    }

    /// Starts up to `count` more workers, in free slots; returns how many
    /// started.
    pub(crate) fn start(inner: &Arc<Inner>, count: usize) -> io::Result<usize> {
        let mut started = 0;
        while started < count && Self::start_one(inner)? {
            started += 1;
        }
        Ok(started)
    }

    /// Starts a worker in the next free slot; `false` when none is left.
    fn start_one(inner: &Arc<Inner>) -> io::Result<bool> {
        let workers = &inner.workers;
        let index = workers.claimed.fetch_add(1, Ordering::AcqRel);
        if index >= workers.slots.len() {
            workers.claimed.fetch_sub(1, Ordering::AcqRel);
            return Ok(false);
        }
        // A shard that cannot be made leaves its slot empty; placement
        // skips it.
        let shard = Shard::new(workers.start.limits)?;
        let shared = Arc::clone(&shard.shared);
        let _ = workers.slots[index].set(Arc::clone(&shared));
        workers.running.fetch_add(1, Ordering::AcqRel);
        let cores = &workers.start.cores;
        let core = (!cores.is_empty()).then(|| cores[index % cores.len()]);
        let exits = workers.exits.clone();
        let worker_inner = Arc::clone(inner);
        let spawned = std::thread::Builder::new()
            .name((workers.start.name)())
            .stack_size(workers.start.stack)
            .spawn(move || {
                /// Reports the worker's exit once its shard is dropped, even
                /// when the loop ends by a panic.
                struct Exit(kovan_channel::unbounded::Sender<()>);
                impl Drop for Exit {
                    fn drop(&mut self) {
                        self.0.send(());
                    }
                }
                let _exit = Exit(exits);
                if let Some(core) = core {
                    super::runtime::pin_to(core);
                }
                shard.set_owner(worker_inner.id(), index);
                let _runtime = context::set_runtime(&worker_inner);
                let _current = context::enter(&shard);
                shard.block_on(shard.shared.stopped());
            });
        match spawned {
            Ok(thread) => {
                lock(&workers.threads).push(thread);
                Ok(true)
            }
            Err(err) => {
                // The shard went with the closure; its slot takes no tasks.
                shared.stop();
                workers.running.fetch_sub(1, Ordering::AcqRel);
                Err(err)
            }
        }
    }

    /// Takes one running worker off the count, unless that would go below
    /// the minimum.
    fn claim_stop(&self) -> bool {
        let mut running = self.running.load(Ordering::Acquire);
        while running > self.min {
            match self.running.compare_exchange_weak(
                running,
                running - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(now) => running = now,
            }
        }
        false
    }

    /// Stops the last worker that takes tasks, unless no more than the
    /// minimum run; returns whether one stopped.
    pub(crate) fn stop_one(&self) -> bool {
        if !self.claim_stop() {
            return false;
        }
        match self.shards().filter(|s| s.takes_tasks()).last() {
            Some(shard) => {
                shard.drain();
                true
            }
            None => {
                self.running.fetch_add(1, Ordering::AcqRel);
                false
            }
        }
    }

    /// Stops `shard`, a worker of this runtime, unless no more than the
    /// minimum run; returns whether it stopped.
    pub(crate) fn stop(&self, shard: &Shared) -> bool {
        if !shard.takes_tasks() || !self.claim_stop() {
            return false;
        }
        shard.drain();
        true
    }

    /// Stops every worker now, tasks and all.
    pub(crate) fn stop_all(&self) {
        self.shards().for_each(|s| s.stop());
    }

    /// The worker threads started so far, taken for joining.
    pub(crate) fn take_threads(&self) -> Vec<JoinHandle<()>> {
        core::mem::take(&mut *lock(&self.threads))
    }
}

/// The list of threads; a panic while it was held (pushing a handle) left
/// it whole.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A random number below `n`, from a per-thread xorshift generator.
pub(crate) fn random_below(n: usize) -> usize {
    use core::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new({
            // Seeded from the thread's own address and the clock: distinct
            // per thread, and nothing depends on its quality but balance.
            let local = 0u8;
            let addr = &local as *const u8 as u64;
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            (addr ^ time.rotate_left(32)) | 1
        });
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        // Multiply-shift: unbiased enough for placement, no division.
        ((u128::from(x) * n as u128) >> 64) as usize
    })
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::Workers;
    use crate::runtime::{Builder, Runtime};

    fn runtime(workers: usize, max: usize) -> Runtime {
        Builder::new()
            .worker_threads(workers)
            .max_worker_threads(max)
            .pin_workers(false)
            .build()
            .unwrap()
    }

    fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "{what} never happened");
            std::thread::yield_now();
        }
    }

    /// The threads that tasks spawned from outside run on.
    fn threads_used(rt: &Runtime, tasks: usize) -> HashSet<std::thread::ThreadId> {
        let ids: Vec<_> = (0..tasks)
            .map(|_| {
                rt.spawn(async {
                    crate::time::sleep(Duration::from_millis(1)).await;
                    std::thread::current().id()
                })
            })
            .collect();
        rt.block_on(async {
            let mut set = HashSet::new();
            for id in ids {
                set.insert(id.await);
            }
            set
        })
    }

    #[test]
    fn more_workers_start_up_to_the_maximum_and_take_tasks() {
        let rt = runtime(1, 3);
        assert_eq!(threads_used(&rt, 32).len(), 1);
        assert_eq!(
            Workers::start(rt.inner(), 5).unwrap(),
            2,
            "two fit under the maximum"
        );
        assert_eq!(rt.inner().workers.running(), 3);
        assert_eq!(Workers::start(rt.inner(), 1).unwrap(), 0, "none is left");
        assert!(threads_used(&rt, 300).len() > 1, "new workers take tasks");
    }

    #[test]
    fn a_stop_leaves_the_minimum_running() {
        let rt = runtime(1, 3);
        Workers::start(rt.inner(), 2).unwrap();
        assert!(rt.inner().workers.stop_one());
        assert!(rt.inner().workers.stop_one());
        assert!(!rt.inner().workers.stop_one(), "the minimum stays");
        assert_eq!(rt.inner().workers.running(), 1);
        // Every new task goes to the one worker left.
        assert_eq!(threads_used(&rt, 64).len(), 1);
    }

    #[test]
    fn a_stopped_worker_finishes_its_task_then_exits() {
        let rt = runtime(1, 2);
        Workers::start(rt.inner(), 1).unwrap();
        let workers = &rt.inner().workers;
        let second = workers.shards().nth(1).map(Arc::clone).unwrap();
        let release = Arc::new(AtomicBool::new(false));
        // A task held on the second worker until released: tasks are
        // spawned until one lands there, and only that one waits.
        let held = (0..1000)
            .find_map(|_| {
                let (r, s) = (Arc::clone(&release), Arc::clone(&second));
                let (tell, landed) = std::sync::mpsc::channel();
                let task = rt.spawn(async move {
                    let here = crate::rt::context::current(|c| c.is_some_and(|shard| shard.is(&s)));
                    let _ = tell.send(here);
                    while here && !r.load(Ordering::SeqCst) {
                        crate::time::sleep(Duration::from_millis(1)).await;
                    }
                    here
                });
                let here = landed.recv_timeout(Duration::from_secs(10)).unwrap();
                here.then_some(task)
            })
            .expect("no task landed on the second worker");
        assert!(workers.stop(&second));
        assert!(!second.takes_tasks());
        // It still runs its task.
        assert!(!second.is_stopped());
        release.store(true, Ordering::SeqCst);
        assert!(rt.block_on(held));
        eventually("the stopped worker's exit", || second.is_stopped());
    }

    #[test]
    fn a_source_on_a_worker_that_exits_registers_again_where_it_is_polled() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        let rt = runtime(2, 2);
        let (mut peer, mine) = UnixStream::pair().unwrap();
        let handle = Arc::new(crate::io::Handle::new(mine).unwrap());
        // A receive on a worker registers the source with that worker.
        let h = Arc::clone(&handle);
        let first = rt.spawn(async move {
            let mut buf = [0u8; 4];
            let n = h.recv(&mut buf).await.unwrap();
            (buf, n, std::thread::current().id())
        });
        peer.write_all(b"one!").unwrap();
        let (_, n, _) = rt.block_on(first);
        assert_eq!(n, 4);
        // Every worker that holds a registration goes; the next receive,
        // polled from this thread, registers the source again.
        for shard in rt.inner().workers.shards() {
            shard.stop();
        }
        eventually("the workers' exit", || {
            rt.inner().workers.shards().all(|s| s.is_stopped())
        });
        let h = Arc::clone(&handle);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            peer.write_all(b"two!").unwrap();
            peer
        });
        let mut buf = [0u8; 4];
        let n = crate::runtime::block_on(async {
            crate::time::timeout(Duration::from_secs(10), h.recv(&mut buf)).await
        })
        .expect("the receive never completed")
        .unwrap();
        assert_eq!(&buf[..n], b"two!");
        drop(writer.join().unwrap());
    }
}

#[cfg(all(test, not(loom)))]
mod placement_tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use crate::runtime::{Builder, Runtime};

    fn runtime(workers: usize) -> Runtime {
        Builder::new()
            .worker_threads(workers)
            .pin_workers(false)
            .build()
            .unwrap()
    }

    /// Spawns `count` tasks from this thread, each held until `release`;
    /// returns how many landed on each worker thread.
    fn spread(rt: &Runtime, count: usize, release: &Arc<AtomicBool>) -> Vec<usize> {
        let (tell, landed) = std::sync::mpsc::channel();
        let tasks: Vec<_> = (0..count)
            .map(|_| {
                let (tell, release) = (tell.clone(), Arc::clone(release));
                rt.spawn(async move {
                    let _ = tell.send(std::thread::current().id());
                    while !release.load(Ordering::SeqCst) {
                        crate::time::sleep(Duration::from_millis(1)).await;
                    }
                })
            })
            .collect();
        let mut per_thread: HashMap<_, usize> = HashMap::new();
        for _ in 0..count {
            let id = landed.recv_timeout(Duration::from_secs(10)).unwrap();
            *per_thread.entry(id).or_default() += 1;
        }
        tasks.into_iter().for_each(|t| t.detach());
        let mut counts: Vec<_> = per_thread.into_values().collect();
        counts.sort_unstable();
        counts
    }

    #[test]
    fn spawns_from_outside_the_runtime_stay_together() {
        let rt = runtime(4);
        let release = Arc::new(AtomicBool::new(true));
        // Below the margin: every task goes to the home shard.
        assert_eq!(spread(&rt, 20, &release), [20]);
    }

    #[test]
    fn spawns_from_outside_spread_once_a_shard_is_a_margin_ahead() {
        let rt = runtime(4);
        let release = Arc::new(AtomicBool::new(false));
        let counts = spread(&rt, 400, &release);
        release.store(true, Ordering::SeqCst);
        assert_eq!(counts.len(), 4, "every worker took tasks: {counts:?}");
        let (min, max) = (counts[0], counts[counts.len() - 1]);
        assert!(
            max - min <= 3 * 32,
            "the load stays within the margin: {counts:?}"
        );
    }
}
