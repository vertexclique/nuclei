//! The blocking pool: threads for work that blocks, so it never holds a
//! shard up.
//!
//! A job is a task whose future runs the closure in one poll. It goes into
//! the pool's queue, the shard inbox's queue linked through the tasks
//! (`rt::inbox`), so a submit allocates nothing; the pool's threads take
//! from it one at a time (the queue's taker's role, held for one take).
//! Threads are started on demand up to a maximum and leave after a
//! keep-alive with nothing to do.
//!
// vertexia: the queue is unbounded, as the established pool's is: a cap in bytes needs a way to
// refuse a job, and `spawn_blocking` cannot fail (running it inline would block a shard). A
// fallible variant that refuses past a byte budget is the upgrade path.
//
//! An idle thread waits in a slot of its own, one of `max_threads` made
//! with the pool: free, busy, idle, or woken. No job is ever left without a
//! thread. A submitter pushes its job, then, if the idle count says one
//! waits, claims an idle slot (idle to woken) and unparks its thread; if it
//! claims none, it starts a thread while there is room. A thread with
//! nothing to take counts itself idle, marks its slot idle, and looks at the
//! queue once more, taking its slot back if a job is there (unless a
//! submitter claimed it first). At its keep-alive it frees its slot (unless
//! claimed meanwhile), leaves the thread count, and looks once more, taking
//! back a job a submitter found no room to start a thread for. Every one of
//! these accesses is sequentially consistent: either a thread's look sees a
//! job, or the job's submitter sees the thread idle. proofs/tla/pool checks
//! it at the grain of these steps.
//!
//! Beside the slots is a bitmap with one bit per slot. A thread sets its own
//! bit after it marks its slot idle and before its look at the queue, and
//! clears it after it takes its slot back, and before the compare-and-swap
//! that ends its wait: only a slot's own thread writes its bit. A submitter
//! reads the bitmap one word per load, and tries the compare-and-swap (idle
//! to woken) on the slots of set bits only; a stale bit loses that swap and
//! it goes on to the next. With no bit leading to a claim it starts a thread
//! while there is room. That is proofs/tla/pool/PoolIndex.tla.
//!
//! A slot names its thread for the submitters that wake it through a
//! `kovan` pointer read under a guard: a slot a thread left may be another
//! thread's by the time a slow submitter reads it, so the name a thread
//! replaces is retired, not freed. That is one node per thread started,
//! none per job.

use core::sync::atomic::AtomicU8;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kovan::{Atomic, RetiredNode, Shared as Ptr, alloc_node, drop_node, pin, retire_node};

use super::inbox::{Inbox, Pop};
use super::task::{self, Runnable, Schedule, ScheduleInfo, Task};
use crate::sync::internal::{AtomicU64, AtomicUsize, Ordering};

/// The pool's settings.
#[derive(Clone)]
pub(crate) struct PoolConfig {
    pub(crate) max_threads: usize,
    /// The runtime's thread hooks; a pool thread calls start and stop.
    pub(crate) hooks: super::tuning::ThreadHooks,
    pub(crate) keep_alive: Duration,
    pub(crate) stack_size: Option<usize>,
    pub(crate) name: Arc<dyn Fn() -> String + Send + Sync>,
    /// The mask the runtime was built with; a pool thread runs on it.
    pub(crate) cpu_mask: Option<super::runtime::CpuMask>,
}

/// A slot's states.
const FREE: u8 = 0;
const BUSY: u8 = 1;
const IDLE: u8 = 2;
const WOKEN: u8 = 3;

/// Slots per word of the index.
const WORD: usize = u64::BITS as usize;

/// The thread a slot names, as a node `kovan` reclaims.
#[repr(C)]
struct Named {
    retired: RetiredNode,
    thread: std::thread::Thread,
}

/// Where a pool thread waits when it has nothing to take.
struct Slot {
    state: AtomicU8,
    thread: Atomic<Named>,
}

/// A runtime's blocking pool.
pub(crate) struct Pool {
    /// The jobs, linked through their tasks; its taker's role is free
    /// between takes.
    queue: Inbox,
    slots: Box<[Slot]>,
    /// One bit per slot, set while its thread waits idle; written by that
    /// thread alone.
    index: Box<[AtomicU64]>,
    /// Where a submitter's scan of the index starts.
    hint: AtomicUsize,
    threads: AtomicUsize,
    idle: AtomicUsize,
    /// Jobs submitted and not yet run to their end: a paused clock does not
    /// move by itself while one is in flight.
    jobs: Arc<AtomicUsize>,
    /// The runtime the pool belongs to, told when its last job ends.
    owner: std::sync::OnceLock<std::sync::Weak<super::runtime::Inner>>,
    config: PoolConfig,
}

/// The schedule of a blocking job: into the pool's queue.
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
        let queue = Inbox::new();
        // SAFETY: its maker holds the role; the pool's threads take turns.
        unsafe { queue.release() };
        let n = config.max_threads.max(1);
        let slots = (0..n)
            .map(|_| Slot {
                state: AtomicU8::new(FREE),
                thread: Atomic::null(),
            })
            .collect();
        let index = (0..n.div_ceil(WORD)).map(|_| AtomicU64::new(0)).collect();
        Arc::new(Self {
            queue,
            slots,
            index,
            hint: AtomicUsize::new(0),
            threads: AtomicUsize::new(0),
            idle: AtomicUsize::new(0),
            jobs: Arc::new(AtomicUsize::new(0)),
            owner: std::sync::OnceLock::new(),
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

    /// Makes `owner` the runtime told when the pool's last job ends.
    pub(crate) fn set_owner(&self, owner: std::sync::Weak<super::runtime::Inner>) {
        let _ = self.owner.set(owner);
    }

    /// The count of [`jobs`](Self::jobs), for a shard that spins while one may wake it.
    pub(crate) fn jobs_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.jobs)
    }

    /// Jobs submitted and not yet run to their end.
    pub(crate) fn jobs(&self) -> usize {
        self.jobs.load(Ordering::SeqCst)
    }

    /// Runs a job, then counts it out. Its awaiter was woken during the run,
    /// before the count drops, so a paused clock that sees no job in flight
    /// sees the woken shard's work.
    fn run(&self, runnable: Runnable) {
        runnable.run();
        if self.jobs.fetch_sub(1, Ordering::SeqCst) == 1
            && let Some(owner) = self.owner.get().and_then(std::sync::Weak::upgrade)
        {
            super::paused::try_advance(&owner, None);
        }
    }

    fn submit(self: &Arc<Self>, runnable: Runnable) {
        self.jobs.fetch_add(1, Ordering::SeqCst);
        self.queue.push(runnable);
        if self.idle.load(Ordering::SeqCst) > 0 && self.wake_one() {
            return;
        }
        self.start_thread();
    }

    /// Claims an idle slot and unparks its thread; `false` if no set bit of
    /// the index led to a claim.
    fn wake_one(&self) -> bool {
        let n = self.slots.len();
        let words = self.index.len();
        let from = self.hint.fetch_add(1, Ordering::Relaxed) % n;
        let (first, low) = (from / WORD, from % WORD);
        // The hint's word twice: its bits from the hint on, then, after the
        // others, its bits before it.
        for step in 0..=words {
            let w = (first + step) % words;
            let mut bits = self.index[w].load(Ordering::SeqCst);
            if step == 0 {
                bits &= !0u64 << low;
            } else if step == words {
                bits &= !(!0u64 << low);
            }
            while bits != 0 {
                let i = w * WORD + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if self.claim(i) {
                    return true;
                }
            }
        }
        false
    }

    /// Claims slot `i` if it is idle (the one step that takes a thread),
    /// counts it out and unparks its thread.
    fn claim(&self, i: usize) -> bool {
        let slot = &self.slots[i];
        if slot
            .state
            .compare_exchange(IDLE, WOKEN, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        self.idle.fetch_sub(1, Ordering::SeqCst);
        let guard = pin();
        // SAFETY: the guard keeps the name alive while it is read; the
        // thread stored it before it marked its slot idle.
        if let Some(named) = unsafe { slot.thread.load(Ordering::Acquire, &guard).as_ref() } {
            named.thread.unpark();
        }
        true
    }

    /// Sets slot `i`'s bit; called by its thread alone.
    fn set_bit(&self, i: usize) {
        self.index[i / WORD].fetch_or(1 << (i % WORD), Ordering::SeqCst);
    }

    /// Clears slot `i`'s bit; called by its thread alone.
    fn clear_bit(&self, i: usize) {
        self.index[i / WORD].fetch_and(!(1 << (i % WORD)), Ordering::SeqCst);
    }

    /// The next job, taking the queue's role for one take; `None` when the
    /// queue is empty.
    fn take(&self) -> Option<Runnable> {
        let mut spins = 0u32;
        loop {
            if self.queue.try_take() {
                // SAFETY: the role is this thread's, for this one take.
                let popped = unsafe {
                    let popped = self.queue.pop();
                    self.queue.release();
                    popped
                };
                match popped {
                    Pop::Item(runnable) => return Some(runnable),
                    Pop::Empty => return None,
                    Pop::Busy => {}
                }
            } else if self.queue.is_empty() {
                // Another thread takes the last job.
                return None;
            }
            // Another taker, or a push between two of its steps: a moment.
            spins += 1;
            if spins.is_multiple_of(64) {
                std::thread::yield_now();
            } else {
                core::hint::spin_loop();
            }
        }
    }

    /// Starts a thread if the pool has room for one.
    fn start_thread(self: &Arc<Self>) {
        if !self.count_thread() {
            return;
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

    /// Counts one more thread, if the pool has room for it.
    fn count_thread(&self) -> bool {
        let mut n = self.threads.load(Ordering::SeqCst);
        loop {
            if n >= self.config.max_threads {
                return false;
            }
            match self
                .threads
                .compare_exchange_weak(n, n + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(actual) => n = actual,
            }
        }
    }

    /// A thread could not be started. With none alive, the queued jobs would
    /// wait forever: they run here instead, on the submitting thread.
    fn cannot_start(&self, _err: io::Error) {
        if self.threads.load(Ordering::SeqCst) == 0 {
            while let Some(runnable) = self.take() {
                self.run(runnable);
            }
        }
    }

    /// A free slot, taken for this thread and named after it. One is always
    /// free for a counted thread: a thread frees its slot before it leaves
    /// the count.
    fn claim_slot(&self) -> usize {
        let n = self.slots.len();
        let mut spins = 0u32;
        loop {
            for i in 0..n {
                let slot = &self.slots[i];
                if slot
                    .state
                    .compare_exchange(FREE, BUSY, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let named = alloc_node(Named {
                        retired: RetiredNode::new(),
                        thread: std::thread::current(),
                    });
                    let guard = pin();
                    // SAFETY: a fresh node, published here once.
                    let new = unsafe { Ptr::from_raw(named) };
                    let old = slot.thread.swap(new, Ordering::AcqRel, &guard);
                    if !old.is_null() {
                        // SAFETY: unlinked by the swap; a submitter may still
                        // read it under its guard, so it is retired once.
                        unsafe { retire_node(old.as_raw()) };
                    }
                    return i;
                }
            }
            // A leaving thread is between freeing its slot and leaving the
            // count: a moment.
            spins += 1;
            if spins.is_multiple_of(64) {
                std::thread::yield_now();
            } else {
                core::hint::spin_loop();
            }
        }
    }

    /// A pool thread's life.
    fn work(self: Arc<Self>) {
        let _alive = Alive::new();
        // A thread inherits its creator's mask, and a pinned worker often
        // creates pool threads.
        if let Some(mask) = &self.config.cpu_mask {
            super::runtime::apply_mask(mask);
        }
        let _stopping = self.config.hooks.started();
        let mut me = self.claim_slot();
        loop {
            while let Some(runnable) = self.take() {
                self.run(runnable);
            }
            let slot = &self.slots[me];
            self.idle.fetch_add(1, Ordering::SeqCst);
            slot.state.store(IDLE, Ordering::SeqCst);
            self.set_bit(me);
            // A job pushed while the submitter saw this thread busy.
            if !self.queue.is_empty() {
                if slot
                    .state
                    .compare_exchange(IDLE, BUSY, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    self.idle.fetch_sub(1, Ordering::SeqCst);
                } else {
                    // Claimed: the submitter counted it out.
                    slot.state.store(BUSY, Ordering::SeqCst);
                }
                self.clear_bit(me);
                continue;
            }
            let deadline = Instant::now() + self.config.keep_alive;
            loop {
                if slot.state.load(Ordering::SeqCst) == WOKEN {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                std::thread::park_timeout(deadline - now);
            }
            // Before the slot leaves idle: a freed slot may be another
            // thread's at once, and a late clear would erase its bit.
            self.clear_bit(me);
            if slot
                .state
                .compare_exchange(IDLE, FREE, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                // Woken, and counted out by its waker.
                slot.state.store(BUSY, Ordering::SeqCst);
                continue;
            }
            self.idle.fetch_sub(1, Ordering::SeqCst);
            self.threads.fetch_sub(1, Ordering::SeqCst);
            // A job sent while this thread still counted toward the maximum
            // found no room for a new thread: it is this one's.
            if !self.queue.is_empty() && self.count_thread() {
                me = self.claim_slot();
                continue;
            }
            return;
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let guard = pin();
        for slot in self.slots.iter() {
            let named = slot.thread.load(Ordering::Acquire, &guard);
            if !named.is_null() {
                // SAFETY: the pool is gone, so no submitter reads the slot;
                // the node came from `alloc_node`, let go of once, here.
                unsafe { drop_node(named.as_raw()) };
            }
        }
    }
}

/// Threads of every blocking pool alive now.
static ALIVE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Runs `call` on the blocking pool and answers its result; a call whose task
/// was lost (its runtime shut down) is an error, not a panic.
pub(crate) async fn call<T, F>(call: F) -> io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    match crate::runtime::spawn_blocking(call).join().await {
        Ok(result) => result.inspect_err(crate::rt::counters::io_error),
        Err(lost) => Err(lost.into()),
    }
}

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
            hooks: crate::rt::tuning::ThreadHooks::default(),
            keep_alive,
            stack_size: None,
            name: Arc::new(|| "test-blocking".to_string()),
            cpu_mask: None,
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

    /// One thread with a long keep-alive, and each job submitted the moment
    /// the last one's closure returns, with the submitter spinning on it: the
    /// submit lands as the thread finds the queue empty and goes idle. A job
    /// it missed would wait out the keep-alive; each must run at once.
    #[test]
    fn a_job_submitted_as_the_thread_goes_idle_is_not_missed() {
        use std::sync::atomic::Ordering::SeqCst;
        let pool = pool(1, Duration::from_secs(30));
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for i in 0..20_000usize {
            let ran = Arc::clone(&done);
            pool.spawn(move || ran.store(i + 1, SeqCst)).detach();
            let deadline = Instant::now() + Duration::from_secs(5);
            while done.load(SeqCst) != i + 1 {
                assert!(
                    Instant::now() < deadline,
                    "job {i} waited for the keep-alive"
                );
                core::hint::spin_loop();
            }
        }
        assert_eq!(pool.threads(), 1);
    }

    /// Submitters on threads against two pool threads whose keep-alive runs
    /// out between jobs: every job runs once, through every wake, look and
    /// leave of the pool's threads (proofs/tla/pool).
    #[test]
    fn every_job_runs_once_while_threads_come_and_go() {
        use std::sync::atomic::Ordering::SeqCst;
        const SUBMITTERS: usize = 4;
        const EACH: usize = 2_000;
        let pool = pool(2, Duration::from_millis(1));
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let submitters: Vec<_> = (0..SUBMITTERS)
            .map(|s| {
                let (pool, ran) = (Arc::clone(&pool), Arc::clone(&ran));
                std::thread::spawn(move || {
                    for i in 0..EACH {
                        let ran = Arc::clone(&ran);
                        let task = pool.spawn(move || ran.fetch_add(1, SeqCst));
                        // Some jobs awaited, some left to run on their own,
                        // and gaps that let the threads time out.
                        if (s + i) % 3 == 0 {
                            crate::sync::internal::block_on(task);
                        } else {
                            task.detach();
                        }
                        if i % 97 == 0 {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                })
            })
            .collect();
        for s in submitters {
            s.join().unwrap();
        }
        wait_for("every job to run", || ran.load(SeqCst) == SUBMITTERS * EACH);
        assert_eq!(ran.load(SeqCst), SUBMITTERS * EACH);
        assert!(pool.threads() <= 2);
        wait_for("the jobs to be counted out", || pool.jobs() == 0);
    }

    fn bits(pool: &Pool) -> u32 {
        use std::sync::atomic::Ordering::SeqCst;
        pool.index.iter().map(|w| w.load(SeqCst).count_ones()).sum()
    }

    /// Submitters on threads against a pool at its cap, with jobs that block
    /// long enough to keep it there: every job runs once, over one word of
    /// the index and over several (proofs/tla/pool/PoolIndex.tla).
    #[test]
    fn every_job_runs_once_at_the_cap() {
        use std::sync::atomic::Ordering::SeqCst;
        const SUBMITTERS: usize = 8;
        const EACH: usize = 500;
        for max in [8, 70] {
            let pool = pool(max, Duration::from_secs(5));
            let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let submitters: Vec<_> = (0..SUBMITTERS)
                .map(|_| {
                    let (pool, ran) = (Arc::clone(&pool), Arc::clone(&ran));
                    std::thread::spawn(move || {
                        for i in 0..EACH {
                            let ran = Arc::clone(&ran);
                            let task = pool.spawn(move || {
                                if i % 8 == 0 {
                                    std::thread::sleep(Duration::from_micros(200));
                                }
                                ran.fetch_add(1, SeqCst)
                            });
                            if i % 2 == 0 {
                                crate::sync::internal::block_on(task);
                            } else {
                                task.detach();
                            }
                        }
                    })
                })
                .collect();
            for s in submitters {
                s.join().unwrap();
            }
            wait_for("every job to run", || ran.load(SeqCst) == SUBMITTERS * EACH);
            assert_eq!(ran.load(SeqCst), SUBMITTERS * EACH);
            assert!(pool.threads() <= max);
            wait_for("the jobs to be counted out", || pool.jobs() == 0);
        }
    }

    /// Submitters racing a thread as it goes idle and takes its slot back:
    /// each job is sent the moment the last one's closure returned, by one of
    /// two submitters, with a long keep-alive. A job the index lost would
    /// wait it out.
    #[test]
    fn a_thread_going_idle_while_submitters_race_loses_no_job() {
        use std::sync::atomic::Ordering::SeqCst;
        let pool = pool(2, Duration::from_secs(30));
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let submitters: Vec<_> = (0..2)
            .map(|_| {
                let (pool, done) = (Arc::clone(&pool), Arc::clone(&done));
                std::thread::spawn(move || {
                    for _ in 0..5_000 {
                        let before = done.load(SeqCst);
                        let ran = Arc::clone(&done);
                        pool.spawn(move || ran.fetch_add(1, SeqCst)).detach();
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while done.load(SeqCst) == before {
                            assert!(Instant::now() < deadline, "a job waited for the keep-alive");
                            core::hint::spin_loop();
                        }
                    }
                })
            })
            .collect();
        for s in submitters {
            s.join().unwrap();
        }
        wait_for("every job to run", || done.load(SeqCst) == 10_000);
        wait_for("the jobs to be counted out", || pool.jobs() == 0);
    }

    /// The keep-alive exit with the index: threads that leave take their bits
    /// with them, and a job after they left starts a thread again.
    #[test]
    fn the_keep_alive_exit_leaves_no_bit_set() {
        let pool = pool(70, Duration::from_millis(10));
        for _ in 0..3 {
            let tasks: Vec<_> = (0..70)
                .map(|i| {
                    pool.spawn(move || std::thread::sleep(Duration::from_micros(100 * (i % 5))))
                })
                .collect();
            for task in tasks {
                crate::sync::internal::block_on(task);
            }
            wait_for("the idle threads to exit", || pool.threads() == 0);
            assert_eq!(bits(&pool), 0);
            assert_eq!(pool.idle.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }
}
