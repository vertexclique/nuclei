//! Property tests for `nuclei::sync` against sequential models.
//!
//! Each case drives one primitive with a random schedule of acquires,
//! polls, cancellations (dropping a pending future), releases, `try_`
//! calls and storms on a single-threaded executor that polls every woken
//! future until nothing more is woken. A storm releases what is held and
//! lets a barging `try_` call take it before the woken waiter runs, which
//! is what passes a waiter over. After every step the case checks:
//!
//! - **Exclusion and accounting:** permits held and free add up; a writer
//!   holds alone; one upgradable read at most.
//! - **No stranded waiter:** once quiet, nobody waits for what is free.
//!   For a semaphore, the oldest waiter needs more than is free; for a lock,
//!   someone holds it.
//! - **Bounded bypass:** a waiter that is woken and loses is passed over;
//!   it is passed over at most [`MAX_BYPASS`] times, plus once for each
//!   handoff owed to another waiter in the meantime.
//!
//! At the end everything held is released, round after round, and every
//! waiter must finish: nothing is lost and nothing deadlocks, upgrades
//! included. Notify keeps its own exact model: it hands notifications over
//! in order.
//!
//! Single-threaded schedules cover the protocol's decisions; the loom
//! models in `tests/sync_loom.rs` cover the interleavings.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use nuclei::sync::{
    MAX_BYPASS, Mutex, MutexGuard, Notify, RwLock, RwLockReadGuard, RwLockUpgradableReadGuard,
    RwLockWriteGuard, Semaphore, SemaphorePermit,
};
use proptest::prelude::*;

struct Counter(AtomicUsize);

impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A future polled by hand under a waker that counts its wakes.
struct Polled<F: Future> {
    future: Pin<Box<F>>,
    counter: Arc<Counter>,
    waker: Waker,
    seen: usize,
    /// Polls after a wake that came back `Pending`: times passed over.
    losses: usize,
}

impl<F: Future> Polled<F> {
    fn new(future: F) -> Self {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        Self {
            future: Box::pin(future),
            waker: Waker::from(Arc::clone(&counter)),
            counter,
            seen: 0,
            losses: 0,
        }
    }

    fn poll(&mut self) -> Poll<F::Output> {
        self.future
            .as_mut()
            .poll(&mut Context::from_waker(&self.waker))
    }

    fn wakes(&self) -> usize {
        self.counter.0.load(Ordering::SeqCst)
    }

    fn woken(&self) -> bool {
        self.wakes() > self.seen
    }
}

/// Polls every woken task until none is, moving the finished ones'
/// outputs into `done`.
fn run<F: Future>(tasks: &mut Vec<Polled<F>>, done: &mut Vec<F::Output>) {
    for _ in 0..10_000 {
        let Some(at) = tasks.iter().position(Polled::woken) else {
            return;
        };
        let task = &mut tasks[at];
        task.seen = task.wakes();
        match task.poll() {
            Poll::Ready(output) => {
                tasks.remove(at);
                done.push(output);
            }
            Poll::Pending => task.losses += 1,
        }
    }
    panic!("the executor never went quiet: a wake loop");
}

/// One step of a schedule. Indices pick among the live futures or held
/// guards, modulo how many there are.
#[derive(Clone, Debug)]
enum Step {
    Start(usize),
    Poll(usize),
    Cancel(usize),
    Release(usize),
    Try(usize),
    Storm(usize),
    Add(usize),
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let step = prop_oneof![
        3 => (0usize..4).prop_map(Step::Start),
        2 => any::<usize>().prop_map(Step::Poll),
        2 => any::<usize>().prop_map(Step::Cancel),
        3 => any::<usize>().prop_map(Step::Release),
        1 => (0usize..4).prop_map(Step::Try),
        2 => (1usize..24).prop_map(Step::Storm),
        1 => (1usize..3).prop_map(Step::Add),
    ];
    prop::collection::vec(step, 1..80)
}

fn pick(len: usize, i: usize) -> Option<usize> {
    (len != 0).then(|| i % len)
}

/// The bypass bound for one waiter: `MAX_BYPASS`, plus one loss for each
/// handoff owed to another waiter, at most one per waiter ever queued.
fn bypass_bound(queued: usize) -> usize {
    MAX_BYPASS as usize + queued
}

fn run_semaphore(initial: usize, schedule: &[Step], weighted: bool) -> Result<(), TestCaseError> {
    let sem = Semaphore::new(initial);
    let mut total = initial;
    let mut tasks = Vec::new();
    let mut needs: VecDeque<(usize, usize)> = VecDeque::new();
    let mut held: Vec<SemaphorePermit<'_>> = Vec::new();
    let mut queued = 0;
    let need = |n: usize| {
        if weighted {
            (1 + n % 3).min(initial)
        } else {
            1
        }
    };
    for step in schedule {
        let mut done = Vec::new();
        match *step {
            Step::Start(n) => {
                let mut task = Polled::new(sem.acquire(need(n)));
                match task.poll() {
                    Poll::Ready(permit) => held.push(permit),
                    Poll::Pending => {
                        queued += 1;
                        needs.push_back((Arc::as_ptr(&task.counter) as usize, need(n)));
                        tasks.push(task);
                    }
                }
            }
            Step::Poll(i) => {
                if let Some(at) = pick(tasks.len(), i)
                    && let Poll::Ready(permit) = tasks[at].poll()
                {
                    tasks.remove(at);
                    held.push(permit);
                }
            }
            Step::Cancel(i) => {
                if let Some(at) = pick(tasks.len(), i) {
                    tasks.remove(at);
                }
            }
            Step::Release(i) => {
                if let Some(at) = pick(held.len(), i) {
                    drop(held.remove(at));
                }
            }
            Step::Try(n) => held.extend(sem.try_acquire(need(n))),
            Step::Storm(rounds) => {
                for _ in 0..rounds {
                    if held.is_empty() {
                        break;
                    }
                    drop(held.remove(0));
                    held.extend(sem.try_acquire(1));
                    run(&mut tasks, &mut done);
                    held.append(&mut done);
                }
            }
            Step::Add(n) => {
                sem.add_permits(n);
                total += n;
            }
        }
        run(&mut tasks, &mut done);
        held.append(&mut done);
        let live: Vec<usize> = tasks
            .iter()
            .map(|t| Arc::as_ptr(&t.counter) as usize)
            .collect();
        needs.retain(|(id, _)| live.contains(id));
        let out: usize = held.iter().map(SemaphorePermit::count).sum();
        prop_assert_eq!(out + sem.available_permits(), total, "permits leaked");
        if let Some(&(_, front)) = needs.front() {
            prop_assert!(
                front > sem.available_permits(),
                "the oldest waiter needs {} and {} are free, yet it sleeps",
                front,
                sem.available_permits()
            );
        }
        for task in &tasks {
            prop_assert!(
                task.losses <= bypass_bound(queued),
                "a waiter was passed over {} times",
                task.losses
            );
        }
    }
    for _ in 0..1_000 {
        if tasks.is_empty() {
            return Ok(());
        }
        held.clear();
        let mut done = Vec::new();
        run(&mut tasks, &mut done);
        held.append(&mut done);
    }
    prop_assert!(tasks.is_empty(), "{} waiters never finished", tasks.len());
    Ok(())
}

fn run_mutex(schedule: &[Step]) -> Result<(), TestCaseError> {
    let mutex = Mutex::new(());
    let mut tasks = Vec::new();
    let mut held: Vec<MutexGuard<'_, ()>> = Vec::new();
    let mut queued = 0;
    for step in schedule {
        let mut done = Vec::new();
        match *step {
            Step::Start(_) => {
                let mut task = Polled::new(mutex.lock());
                match task.poll() {
                    Poll::Ready(guard) => held.push(guard),
                    Poll::Pending => {
                        queued += 1;
                        tasks.push(task);
                    }
                }
            }
            Step::Poll(i) => {
                if let Some(at) = pick(tasks.len(), i)
                    && let Poll::Ready(guard) = tasks[at].poll()
                {
                    tasks.remove(at);
                    held.push(guard);
                }
            }
            Step::Cancel(i) => {
                if let Some(at) = pick(tasks.len(), i) {
                    tasks.remove(at);
                }
            }
            Step::Release(_) => held.clear(),
            Step::Try(_) => held.extend(mutex.try_lock()),
            Step::Storm(rounds) => {
                for _ in 0..rounds {
                    if held.is_empty() {
                        break;
                    }
                    held.clear();
                    held.extend(mutex.try_lock());
                    run(&mut tasks, &mut done);
                    held.append(&mut done);
                }
            }
            Step::Add(_) => {}
        }
        run(&mut tasks, &mut done);
        held.append(&mut done);
        prop_assert!(held.len() <= 1, "two guards at once");
        prop_assert!(
            tasks.is_empty() || !held.is_empty(),
            "the mutex is free yet {} waiters sleep",
            tasks.len()
        );
        for task in &tasks {
            // One permit: only the oldest waiter is ever woken, and it is
            // the one owed the handoff, so the bound is exact.
            prop_assert!(
                task.losses <= MAX_BYPASS as usize,
                "a waiter was passed over {} times",
                task.losses
            );
        }
    }
    prop_assert!(queued >= tasks.len());
    for _ in 0..1_000 {
        if tasks.is_empty() {
            return Ok(());
        }
        held.clear();
        let mut done = Vec::new();
        run(&mut tasks, &mut done);
        held.append(&mut done);
    }
    prop_assert!(tasks.is_empty(), "{} waiters never finished", tasks.len());
    Ok(())
}

enum Guard<'a> {
    Read(RwLockReadGuard<'a, ()>),
    Write(RwLockWriteGuard<'a, ()>),
    Upgradable(RwLockUpgradableReadGuard<'a, ()>),
}

impl Guard<'_> {
    /// Reaches the value through the guard, as its holder would.
    fn touch(&self) {
        match self {
            Self::Read(guard) => **guard,
            Self::Write(guard) => **guard,
            Self::Upgradable(guard) => **guard,
        }
    }
}

type RwFuture<'a> = Pin<Box<dyn Future<Output = Guard<'a>> + 'a>>;

fn run_rwlock(schedule: &[Step]) -> Result<(), TestCaseError> {
    let lock = RwLock::new(());
    let mut tasks: Vec<Polled<RwFuture<'_>>> = Vec::new();
    let mut held: Vec<Guard<'_>> = Vec::new();
    let mut queued = 0;
    let start = |kind: usize| -> RwFuture<'_> {
        match kind {
            0 | 3 => Box::pin(async { Guard::Read(lock.read().await) }),
            1 => Box::pin(async { Guard::Write(lock.write().await) }),
            _ => Box::pin(async { Guard::Upgradable(lock.upgradable_read().await) }),
        }
    };
    for step in schedule {
        let mut done = Vec::new();
        match *step {
            Step::Start(kind) => {
                // An upgrade of a held upgradable read, when there is one.
                let upgradable = held.iter().position(|g| matches!(g, Guard::Upgradable(_)));
                let future = match (kind, upgradable) {
                    (3, Some(at)) => match held.remove(at) {
                        Guard::Upgradable(up) => {
                            Box::pin(async { Guard::Write(up.upgrade().await) }) as RwFuture<'_>
                        }
                        _ => unreachable!("matched above"),
                    },
                    _ => start(kind),
                };
                let mut task = Polled::new(future);
                match task.poll() {
                    Poll::Ready(guard) => held.push(guard),
                    Poll::Pending => {
                        queued += 1;
                        tasks.push(task);
                    }
                }
            }
            Step::Poll(i) => {
                if let Some(at) = pick(tasks.len(), i)
                    && let Poll::Ready(guard) = tasks[at].poll()
                {
                    tasks.remove(at);
                    held.push(guard);
                }
            }
            Step::Cancel(i) => {
                if let Some(at) = pick(tasks.len(), i) {
                    tasks.remove(at);
                }
            }
            Step::Release(i) => {
                if let Some(at) = pick(held.len(), i) {
                    drop(held.remove(at));
                }
            }
            Step::Try(kind) => match kind {
                0 | 3 => held.extend(lock.try_read().map(Guard::Read)),
                1 => held.extend(lock.try_write().map(Guard::Write)),
                _ => held.extend(lock.try_upgradable_read().map(Guard::Upgradable)),
            },
            Step::Storm(rounds) => {
                for round in 0..rounds {
                    if held.is_empty() {
                        break;
                    }
                    drop(held.remove(0));
                    if round % 2 == 0 {
                        held.extend(lock.try_read().map(Guard::Read));
                    } else {
                        held.extend(lock.try_write().map(Guard::Write));
                    }
                    run(&mut tasks, &mut done);
                    held.append(&mut done);
                }
            }
            Step::Add(_) => {}
        }
        run(&mut tasks, &mut done);
        held.append(&mut done);
        held.iter().for_each(Guard::touch);
        let writers = held.iter().filter(|g| matches!(g, Guard::Write(_))).count();
        let upgradable = held
            .iter()
            .filter(|g| matches!(g, Guard::Upgradable(_)))
            .count();
        prop_assert!(writers <= 1, "two writers");
        prop_assert!(writers == 0 || held.len() == 1, "a writer shares the lock");
        prop_assert!(upgradable <= 1, "two upgradable reads");
        prop_assert!(
            tasks.is_empty() || !held.is_empty(),
            "the lock is free yet {} waiters sleep",
            tasks.len()
        );
        for task in &tasks {
            prop_assert!(
                task.losses <= bypass_bound(queued),
                "a waiter was passed over {} times",
                task.losses
            );
        }
    }
    for _ in 0..1_000 {
        if tasks.is_empty() {
            return Ok(());
        }
        held.clear();
        let mut done = Vec::new();
        run(&mut tasks, &mut done);
        held.append(&mut done);
    }
    prop_assert!(tasks.is_empty(), "{} waiters never finished", tasks.len());
    Ok(())
}

/// Notify's contract: `notify_one` wakes the oldest waiter or stores one
/// permit; `notify_waiters` wakes everyone waiting; a dropped waiter that
/// had been handed a `notify_one` passes it on.
fn run_notify(schedule: &[Step]) -> Result<(), TestCaseError> {
    let notify = Notify::new();
    let mut permit = false;
    let mut queue: VecDeque<usize> = VecDeque::new();
    // Granted waiters, and whether a `notify_one` (rather than a
    // `notify_waiters`) granted them.
    let mut granted: Vec<(usize, bool)> = Vec::new();
    let mut waiting = Vec::new();
    for (id, step) in schedule.iter().enumerate() {
        match *step {
            Step::Start(_) => {
                let mut future = Polled::new(notify.notified());
                let ready_now = queue.is_empty() && std::mem::take(&mut permit);
                prop_assert_eq!(future.poll().is_ready(), ready_now);
                if !ready_now {
                    queue.push_back(id);
                    waiting.push((id, future));
                }
            }
            Step::Poll(i) => {
                if let Some(at) = pick(waiting.len(), i) {
                    let id = waiting[at].0;
                    let is_granted = granted.iter().any(|(g, _)| *g == id);
                    prop_assert_eq!(waiting[at].1.poll().is_ready(), is_granted);
                    if is_granted {
                        granted.retain(|(g, _)| *g != id);
                        waiting.remove(at);
                    }
                }
            }
            Step::Cancel(i) => {
                if let Some(at) = pick(waiting.len(), i) {
                    let (id, future) = waiting.remove(at);
                    drop(future);
                    queue.retain(|q| *q != id);
                    if let Some(g) = granted.iter().position(|(g, _)| *g == id) {
                        let (_, by_one) = granted.remove(g);
                        if by_one {
                            match queue.pop_front() {
                                Some(next) => granted.push((next, true)),
                                None => permit = true,
                            }
                        }
                    }
                }
            }
            Step::Release(_) | Step::Add(_) | Step::Storm(_) => {
                notify.notify_one();
                match queue.pop_front() {
                    Some(next) => granted.push((next, true)),
                    None => permit = true,
                }
            }
            Step::Try(_) => {
                notify.notify_waiters();
                granted.extend(queue.drain(..).map(|id| (id, false)));
            }
        }
        for (id, future) in &waiting {
            let expected = usize::from(granted.iter().any(|(g, _)| g == id));
            prop_assert_eq!(future.wakes(), expected, "waiter {} woken wrongly", id);
        }
    }
    Ok(())
}

proptest! {
    #[test]
    fn a_semaphore_keeps_its_permits_and_bounds_every_bypass(
        initial in 1usize..4,
        schedule in steps(),
    ) {
        run_semaphore(initial, &schedule, true)?;
    }

    #[test]
    fn a_unit_semaphore_keeps_its_permits_and_bounds_every_bypass(
        initial in 1usize..3,
        schedule in steps(),
    ) {
        run_semaphore(initial, &schedule, false)?;
    }

    #[test]
    fn a_mutex_excludes_and_passes_a_waiter_over_at_most_max_bypass_times(schedule in steps()) {
        run_mutex(&schedule)?;
    }

    #[test]
    fn a_rwlock_excludes_never_strands_and_bounds_every_bypass(schedule in steps()) {
        run_rwlock(&schedule)?;
    }

    #[test]
    fn notify_follows_its_model(schedule in steps()) {
        run_notify(&schedule)?;
    }
}

/// The storm the generator relies on does reach the handoff: without it
/// the bypass bound would be checked on schedules that never approach it.
#[test]
fn a_storm_drives_a_waiter_to_its_handoff() {
    let mutex = Mutex::new(());
    let mut held = vec![mutex.try_lock().expect("free")];
    let mut tasks = vec![Polled::new(mutex.lock())];
    assert!(tasks[0].poll().is_pending());
    let mut done = Vec::new();
    for _ in 0..MAX_BYPASS {
        held.clear();
        held.extend(mutex.try_lock());
        run(&mut tasks, &mut done);
    }
    assert_eq!(tasks[0].losses, MAX_BYPASS as usize);
    held.clear();
    assert!(
        mutex.try_lock().is_none(),
        "the release went straight to the owed waiter"
    );
    run(&mut tasks, &mut done);
    assert!(tasks.is_empty() && done.len() == 1, "handed the lock");
}
