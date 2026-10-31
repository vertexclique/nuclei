//! A runtime's worker shards: the slots they fill, starting and stopping
//! one, where `spawn` places a task, and the threads that run them.
//!
//! A runtime has room for a set number of workers over its life, fixed when
//! it is built (four times the workers it starts with unless set
//! otherwise). Slots fill in order as workers start, and a slot once filled
//! keeps its shard, so placement reads them without a lock. A worker asked
//! to stop takes no new task from then on, runs the ones it has to their
//! end, and exits; its slot is not filled again.
//!
//! With the handoff on (`rt::handoff`), a worker's thread is a runner: it
//! runs the shard it is given until the shard stops, or is taken from it
//! while it is blocked in one poll; it then finishes its bound tasks on its
//! own shard, and waits as a spare for the next shard the monitor hands
//! over. Spares never exit while the runtime runs (a thread that exits
//! cancels the ring requests it submitted), and a shutdown waits for every
//! thread but the ones still blocked in the poll their shard was taken in.

use super::rng::random_below;
use core::cell::Cell;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use super::context;
use super::handoff::home::Home;
use super::handoff::life::Life;
use super::handoff::slot::Slot;
use super::handoff::{Gift, Monitor, Owned, Runner, Spare};
use super::runtime::Inner;
use super::shard::{Limits, Shard, Shared, Worked};
use crate::sync::internal::{AtomicPtr, AtomicUsize, Ordering};

thread_local! {
    /// The runtime and the shard the spawns of this thread (one that is not
    /// that runtime's worker) went to last.
    static HOME: Cell<Option<(u64, usize)>> = const { Cell::new(None) };
}

/// How a worker is started.
pub(crate) struct Start {
    pub(crate) limits: Limits,
    pub(crate) tuning: super::tuning::Tuning,
    /// Seeds each worker's generator.
    pub(crate) seed: Option<super::rng::RngSeed>,
    pub(crate) stack: usize,
    pub(crate) name: Arc<dyn Fn() -> String + Send + Sync>,
    /// The cores workers are pinned to, in turn; empty for none.
    pub(crate) cores: Vec<usize>,
    /// A poll this long hands its shard to another thread; `None` for never.
    pub(crate) handoff_after: Option<Duration>,
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
    /// Every thread the runtime started for its workers (and its monitor),
    /// with its life.
    threads: Mutex<Vec<(JoinHandle<()>, Arc<Life>)>>,
    exits: kovan_channel::unbounded::Sender<()>,
    /// The watch over the workers, with the handoff on.
    monitor: Option<Arc<Monitor>>,
    monitor_started: OnceLock<()>,
}

/// A shard for a runner to run: worker place `place`, watched through
/// `slot` with the handoff on.
struct Run {
    place: usize,
    shard: Owned,
    slot: Option<Arc<Slot>>,
}

impl From<Gift> for Run {
    fn from(gift: Gift) -> Self {
        Self {
            place: gift.place,
            shard: gift.shard,
            slot: Some(gift.slot),
        }
    }
}

impl Workers {
    pub(crate) fn new(
        capacity: usize,
        min: usize,
        start: Start,
        exits: kovan_channel::unbounded::Sender<()>,
    ) -> Self {
        let monitor = start
            .handoff_after
            .filter(|_| capacity > 0 && super::handoff::SUPPORTED)
            .map(|after| {
                Arc::new(Monitor::new(
                    after,
                    capacity,
                    super::runtime::allowed_cores(),
                ))
            });
        Self {
            slots: (0..capacity).map(|_| OnceLock::new()).collect(),
            claimed: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            min,
            start,
            threads: Mutex::new(Vec::new()),
            exits,
            monitor,
            monitor_started: OnceLock::new(),
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
        let mut limits = workers.start.limits;
        if workers.monitor.is_some() {
            limits.backend = limits.backend.handed_over();
        }
        #[expect(
            clippy::arc_with_non_send_sync,
            reason = "the count is what is shared: the shard itself moves between threads only at a hand-over (`rt::handoff`)"
        )]
        let shard = Arc::new(Shard::worker(
            limits,
            workers.start.tuning.clone(),
            inner.id(),
            index,
            workers.monitor.clone(),
        )?);
        let shared = Arc::clone(&shard.shared);
        let _ = workers.slots[index].set(Arc::clone(&shared));
        workers.running.fetch_add(1, Ordering::AcqRel);
        let life = Arc::new(Life::new());
        let slot = workers.monitor.as_ref().map(|monitor| {
            let slot = monitor.slot();
            monitor.started(
                index,
                &shard,
                Runner {
                    slot: Arc::clone(&slot),
                    life: Arc::clone(&life),
                },
            );
            slot
        });
        let run = Run {
            place: index,
            shard: Owned(shard),
            slot,
        };
        match Self::spawn_thread(inner, run, life) {
            Ok(()) => {
                Self::start_monitor(inner)?;
                Ok(true)
            }
            Err((err, _)) => {
                // The shard went with the run; its slot takes no tasks.
                shared.stop();
                workers.running.fetch_sub(1, Ordering::AcqRel);
                Err(err)
            }
        }
    }

    /// Starts the monitor's thread, with the handoff on, once.
    fn start_monitor(inner: &Arc<Inner>) -> io::Result<()> {
        let workers = &inner.workers;
        let Some(monitor) = &workers.monitor else {
            return Ok(());
        };
        if workers.monitor_started.set(()).is_err() {
            return Ok(());
        }
        let (monitor, runtime) = (Arc::clone(monitor), Arc::downgrade(inner));
        let exit = Exit(workers.exits.clone());
        let thread = std::thread::Builder::new()
            .name("nuclei-monitor".to_string())
            .spawn(move || {
                let _exit = exit;
                super::handoff::run(&monitor, &runtime);
            })?;
        lock(&workers.threads).push((thread, Arc::new(Life::new())));
        Ok(())
    }

    /// Starts a thread for a shard the monitor took: the gift back if no
    /// thread could start.
    pub(crate) fn spawn_runner(
        inner: &Arc<Inner>,
        gift: Gift,
        life: Arc<Life>,
    ) -> Result<(), Gift> {
        Self::spawn_thread(inner, Run::from(gift), life).map_err(|(_, run)| Gift {
            place: run.place,
            shard: run.shard,
            slot: run.slot.unwrap_or_else(|| inner.workers.monitor_slot()),
        })
    }

    /// A slot for a runner, from the monitor (a fresh one if there is none).
    fn monitor_slot(&self) -> Arc<Slot> {
        self.monitor
            .as_ref()
            .map_or_else(|| Arc::new(Slot::new(true)), |monitor| monitor.slot())
    }

    /// Starts a runner thread for `run`; on failure the error and the run.
    fn spawn_thread(inner: &Arc<Inner>, run: Run, life: Arc<Life>) -> Result<(), (io::Error, Run)> {
        let workers = &inner.workers;
        // The run waits here until the thread takes it, so a thread that
        // cannot start hands it back.
        let handed = Arc::new(Handed(AtomicPtr::new(Box::into_raw(Box::new(run)))));
        let (taken, thread_inner, thread_life) =
            (Arc::clone(&handed), Arc::clone(inner), Arc::clone(&life));
        let spawned = std::thread::Builder::new()
            .name((workers.start.name)())
            .stack_size(workers.start.stack)
            .spawn(move || {
                if let Some(run) = taken.take() {
                    runner(&thread_inner, run, &thread_life);
                }
            });
        match spawned {
            Ok(thread) => {
                lock(&workers.threads).push((thread, life));
                Ok(())
            }
            Err(err) => match handed.take() {
                Some(run) => Err((err, run)),
                // Never: the closure did not run.
                None => Ok(()),
            },
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

    /// Stops every worker now, tasks and all, the monitor and the spares.
    pub(crate) fn stop_all(&self) {
        if let Some(monitor) = &self.monitor {
            monitor.close();
        }
        self.shards().for_each(|s| s.stop());
    }

    /// The runtime's threads started so far, taken for joining: every one
    /// but a thread still blocked in the poll its shard was taken in, which
    /// is left to exit on its own.
    pub(crate) fn take_threads(&self) -> Vec<JoinHandle<()>> {
        core::mem::take(&mut *lock(&self.threads))
            .into_iter()
            .filter(|(_, life)| !life.is_detached())
            .map(|(thread, _)| thread)
            .collect()
    }
}

/// Reports a runtime thread's exit, once what it held is dropped, even when
/// it ends by a panic.
struct Exit(kovan_channel::unbounded::Sender<()>);

impl Drop for Exit {
    fn drop(&mut self) {
        self.0.send(());
    }
}

/// A run handed to a thread that may not start.
struct Handed(AtomicPtr<Run>);

impl Handed {
    fn take(&self) -> Option<Run> {
        let run = self.0.swap(core::ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: the box was given up to this cell, and is taken out once.
        (!run.is_null()).then(|| *unsafe { Box::from_raw(run) })
    }
}

impl Drop for Handed {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// A runner thread: runs `first`, then, with the handoff on, each shard the
/// monitor hands it, until the runtime shuts down.
fn runner(inner: &Arc<Inner>, first: Run, life: &Arc<Life>) {
    let workers = &inner.workers;
    let _exit = Exit(workers.exits.clone());
    let _stopping = workers.start.tuning.hooks.started();
    life.here();
    let _runtime = context::set_runtime(inner);
    let spare = workers
        .monitor
        .as_ref()
        .map(|_| Spare::here(Arc::clone(life)));
    let mut next = Some(first);
    while let Some(run) = next.take() {
        if run_shard(inner, run) == Worked::Detached {
            life.back();
        }
        // A spare now, with the handoff on; else the thread is done.
        if let (Some(monitor), Some(spare)) = (&workers.monitor, &spare) {
            next = monitor.wait_for_shard(spare).map(Run::from);
        }
    }
}

/// Runs the shard of `run` as its worker's runner until it stops or is
/// taken from this thread; then, taken, the thread's bound tasks on its own
/// shard, until none is live or the runtime shuts down.
fn run_shard(inner: &Arc<Inner>, run: Run) -> Worked {
    let workers = &inner.workers;
    let Run {
        place,
        shard: Owned(shard),
        slot,
    } = run;
    let cores = &workers.start.cores;
    if !cores.is_empty() {
        super::runtime::pin_to(cores[place % cores.len()]);
    }
    let _seeded = workers
        .start
        .seed
        .map(|seed| super::rng::Reseeded::new(seed, place as u64 + 1));
    let home = Arc::new(Home::new(Arc::clone(&shard.shared)));
    context::set_home(Arc::clone(&home));
    let worked = {
        let _current = context::enter_watched(&shard, slot.as_deref());
        shard.run_worker()
    };
    if worked == Worked::Detached
        && let Some(monitor) = &workers.monitor
    {
        finish_home(monitor, &home);
    }
    // While the runtime is still this thread's: what the shard hands on as it
    // goes (a port's keeper) goes there. Its last drop runs where its last
    // holder lets go.
    drop(shard);
    worked
}

/// A thread whose shard was taken: its bound tasks finish on its own shard,
/// their home's target moved there first.
#[expect(
    clippy::panic,
    reason = "a thread whose shard cannot start cannot run its bound tasks, which would wait forever"
)]
fn finish_home(monitor: &Monitor, home: &Home) {
    // No bound task, nothing to finish: no own shard is made for it. None
    // can begin meanwhile: only this thread spawns onto its home.
    if home.is_idle() {
        return;
    }
    let own = match context::own_shard() {
        Ok(own) => own,
        Err(err) => panic!("this thread's shard could not start: {err}"),
    };
    home.target().move_to(&own.shared);
    let _current = context::enter(&own);
    #[cfg(port)]
    own.take_port_back();
    own.block_on_then_leave(monitor.home_done(home));
}

/// The list of threads; a panic while it was held (pushing a handle) left
/// it whole.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    fn a_runtime_watches_its_workers_only_with_the_handoff_on() {
        let off = Builder::new()
            .worker_threads(1)
            .pin_workers(false)
            .handoff_after(None)
            .build()
            .unwrap();
        assert!(off.inner().workers.monitor.is_none());
        let on = runtime(1, 1);
        assert!(on.inner().workers.monitor.is_some(), "on by default");
        assert!(on.inner().workers.monitor_started.get().is_some());
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
        // spawned until one lands there, and only that one waits. Each comes
        // from a new thread, whose first spawn picks a shard afresh (a
        // thread's later spawns stay on its home shard).
        let held = (0..1000)
            .find_map(|_| {
                let (r, s) = (Arc::clone(&release), Arc::clone(&second));
                let (tell, landed) = std::sync::mpsc::channel();
                let probe = async move {
                    let here = crate::rt::context::current(|c| c.is_some_and(|shard| shard.is(&s)));
                    let _ = tell.send(here);
                    while here && !r.load(Ordering::SeqCst) {
                        crate::time::sleep(Duration::from_millis(1)).await;
                    }
                    here
                };
                let task =
                    std::thread::scope(|scope| scope.spawn(|| rt.spawn(probe)).join().unwrap());
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
        let rt = runtime(2, 2);
        #[cfg(unix)]
        let (mut peer, mine) = std::os::unix::net::UnixStream::pair().unwrap();
        #[cfg(windows)]
        let (mut peer, mine) = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            (peer, listener.accept().unwrap().0)
        };
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
        .unwrap_or_else(|_| {
            let source = handle.source();
            panic!(
                "the receive never completed: read side {}; write side {}",
                source.read_report(),
                source.write_report()
            )
        })
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

    /// A runtime of `workers` workers, every one started: placement counts
    /// only the workers that claimed their slot, and a thread the system
    /// starts late would take none of a burst spawned at once.
    fn runtime(workers: usize) -> Runtime {
        let rt = Builder::new()
            .worker_threads(workers)
            .pin_workers(false)
            .build()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while rt.inner().workers.shards().count() < workers {
            assert!(
                std::time::Instant::now() < deadline,
                "the workers never started"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        rt
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
