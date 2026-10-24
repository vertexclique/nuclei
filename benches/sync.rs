//! `nuclei::sync` locks against `std::sync::Mutex`.
//!
//! Run with `cargo bench --bench sync`.
//!
//! - **Uncontended**: one acquire and release on one thread, for the
//!   nuclei `Mutex` (both `try_lock` and a `lock()` future polled once),
//!   `RwLock` reads and writes, a one-permit `Semaphore` acquire, and
//!   `std::sync::Mutex`.
//! - **Contended throughput**: N tasks take the same lock, bump a counter
//!   and release it, for N = 2, 8 and 32; one element is one lock and
//!   unlock. Four shapes:
//!   - `std_mutex`: one thread per task, blocking in `lock()`.
//!   - `nuclei_mutex_park`: one thread per task, driving its future
//!     with an executor that parks the thread while the future is pending
//!     and is unparked by the waker.
//!   - `nuclei_mutex_spin`: the same, but a waiting thread spins on its
//!     wake flag (yielding after a short spin) instead of parking, as an
//!     executor does when it keeps polling.
//!   - `nuclei_mutex_one_thread`: all N tasks on one thread, each
//!     holding the lock across one suspension, so every lock but the
//!     first goes through the queue and nothing leaves the thread.
//!     `std::sync::Mutex` has no equivalent: held across a suspension on
//!     one thread it deadlocks.
//!
//! The uncontended rows run twice: with the lock in the benchmark's stack
//! frame, as before, and in a `Box`, where a lock behind an `Arc` or in a
//! heap-allocated structure lives. On the Intel host these were measured on,
//! the same lock-and-unlock loop ran at one of two speeds about 7 cycles
//! apart, for this crate's locks and for `std::sync::Mutex` alike, depending
//! on where the code and the lock landed (perf counters put the slower speed
//! on loops decoded entirely from the micro-op cache, with both locked
//! instructions microcoded). Two placements of each lock make one lucky or
//! unlucky layout less likely to decide the comparison.
//!
//! The locks barge: a caller takes a free lock even while others wait. A
//! caller that finds the lock held while nobody is queued spins a little
//! first, and a waiter that loses its retry spins on its own node before it
//! sleeps (see `nuclei::sync`), so the releasing thread takes the lock
//! straight back, as with `std::sync::Mutex`. A waiter passed over
//! `MAX_BYPASS` times is handed the lock directly, which costs a context
//! switch for that one acquisition.
//!
//! **Fairness.** Unfairness can buy throughput: a lock that lets one thread
//! keep it while the others wait moves the line less. Each contended row
//! gives every task a fixed share of the work, which hides that. Run the
//! binary with `SYNC_FAIRNESS=<seconds>` instead of the criterion arguments
//! to count, in a window of that length, how many acquisitions each task
//! made in every contended shape: it prints the throughput beside the
//! fewest and the most acquisitions of one task.
//!
//! No async runtime is involved: the executors are the functions below.

mod start {
    //! A common start for benches that time several threads at once.
    //!
    //! The clock must start before any timed operation can run. A single
    //! barrier that the coordinator leaves and then reads the clock after does
    //! not hold that: the workers leave the same barrier and start working while
    //! the coordinator may still be descheduled, so their first operations land
    //! before the window opens and the time per operation reads low. Here the
    //! workers wait at two barriers: the first says every worker is ready, the
    //! clock is read, and only then does the second let them go, so every timed
    //! operation is inside the window.

    use std::sync::{Barrier, OnceLock};
    use std::time::Instant;

    /// The line `workers` threads wait at until the coordinator starts the
    /// clock. Used once: one line per timed run.
    pub struct StartLine {
        ready: Barrier,
        release: Barrier,
        at: OnceLock<Instant>,
    }

    impl StartLine {
        pub fn new(workers: usize) -> Self {
            StartLine {
                ready: Barrier::new(workers + 1),
                release: Barrier::new(workers + 1),
                at: OnceLock::new(),
            }
        }

        /// A worker's side: wait until the clock runs, and return when it
        /// started, so a worker's own deadline is measured from the same instant
        /// as the coordinator's window.
        pub fn wait(&self) -> Instant {
            self.ready.wait();
            self.release.wait();
            *self
                .at
                .get()
                .expect("the coordinator starts the clock before it lets the workers go")
        }

        /// The coordinator's side, once every worker is spawned: wait until all
        /// are at the line, start the clock, then let them go.
        pub fn start(&self) -> Instant {
            self.all_ready();
            self.go()
        }

        /// The first half of [`Self::start`], for a coordinator that reads
        /// something else (a CPU counter) with every worker parked at the line.
        pub fn all_ready(&self) {
            self.ready.wait();
        }

        /// The second half of [`Self::start`]: start the clock, then let the
        /// workers go.
        pub fn go(&self) -> Instant {
            let now = Instant::now();
            let _ = self.at.set(now);
            self.release.wait();
            now
        }
    }

    /// How many of `total` calls thread `t` of `threads` runs: `total / threads`,
    /// one more for the first `total % threads` threads, so the shares add up to
    /// exactly `total`. A criterion `iter_custom` divides its time by the
    /// iterations it asked for; every thread running `total / threads` calls, at
    /// least one, would time fewer calls than that (or more, below one call per
    /// thread) and misstate the time per call.
    pub fn share(total: u64, threads: usize, t: usize) -> u64 {
        let threads = threads as u64;
        total / threads + u64::from((t as u64) < total % threads)
    }
}

use std::future::Future;
use std::hint::black_box;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use nuclei::sync::{Mutex, RwLock, Semaphore};

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

/// How a task's thread waits for its waker.
#[derive(Clone, Copy)]
enum Executor {
    Park,
    Spin,
}

/// Drives `future` on this thread, waiting the `executor` way while it
/// is pending.
///
/// Always inlined: an executor polls a task's future where the task keeps
/// it. Left to the compiler, this function was kept out of line once the
/// lock future's first poll got an inline fast path, and every fresh future
/// was then copied into its frame through the stack, a store-forwarding
/// stall that `perf annotate` put at a third of the function's cycles.
#[inline(always)]
fn block_on<F: Future>(future: F, waker: &Waker, flag: &Flag, executor: Executor) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        match executor {
            Executor::Park => std::thread::park(),
            Executor::Spin => {
                let mut spins = 0u32;
                while !flag.0.swap(false, Ordering::Acquire) {
                    if spins < 128 {
                        std::hint::spin_loop();
                    } else {
                        std::thread::yield_now();
                    }
                    spins += 1;
                }
            }
        }
    }
}

fn uncontended(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_uncontended");
    let waker = Waker::noop();
    let mutex = Mutex::new(0u64);
    group.bench_function("nuclei_mutex_try_lock", |b| {
        b.iter(|| *black_box(&mutex).try_lock().expect("free") += 1)
    });
    group.bench_function("nuclei_mutex_lock_future", |b| {
        b.iter(|| {
            let mut lock = pin!(black_box(&mutex).lock());
            match lock.as_mut().poll(&mut Context::from_waker(waker)) {
                Poll::Ready(mut guard) => *guard += 1,
                Poll::Pending => unreachable!("uncontended"),
            }
        })
    });
    let rwlock = RwLock::new(0u64);
    group.bench_function("nuclei_rwlock_read", |b| {
        b.iter(|| *black_box(&rwlock).try_read().expect("free"))
    });
    group.bench_function("nuclei_rwlock_write", |b| {
        b.iter(|| *black_box(&rwlock).try_write().expect("free") += 1)
    });
    let semaphore = Semaphore::new(1);
    group.bench_function("nuclei_semaphore_acquire", |b| {
        b.iter(|| drop(black_box(&semaphore).try_acquire().expect("free")))
    });
    let std_mutex = std::sync::Mutex::new(0u64);
    group.bench_function("std_mutex_lock", |b| {
        b.iter(|| *black_box(&std_mutex).lock().expect("unpoisoned") += 1)
    });
    let boxed = Box::new(Mutex::new(0u64));
    group.bench_function("nuclei_mutex_try_lock_boxed", |b| {
        b.iter(|| *black_box(&*boxed).try_lock().expect("free") += 1)
    });
    group.bench_function("nuclei_mutex_lock_future_boxed", |b| {
        b.iter(|| {
            let mut lock = pin!(black_box(&*boxed).lock());
            match lock.as_mut().poll(&mut Context::from_waker(waker)) {
                Poll::Ready(mut guard) => *guard += 1,
                Poll::Pending => unreachable!("uncontended"),
            }
        })
    });
    let std_boxed = Box::new(std::sync::Mutex::new(0u64));
    group.bench_function("std_mutex_lock_boxed", |b| {
        b.iter(|| *black_box(&*std_boxed).lock().expect("unpoisoned") += 1)
    });
    group.finish();
}

/// Runs `total` ops spread over `tasks` threads (`start::share`)
/// and returns the wall time from a common start.
fn on_threads(
    tasks: usize,
    total: u64,
    op: impl Fn(&Waker, &Flag) + Send + Sync + 'static,
    park: bool,
) -> Duration {
    let op = Arc::new(op);
    let line = Arc::new(start::StartLine::new(tasks));
    let threads: Vec<_> = (0..tasks)
        .map(|t| {
            let (op, line) = (Arc::clone(&op), Arc::clone(&line));
            let per_task = start::share(total, tasks, t);
            std::thread::spawn(move || {
                let flag = Arc::new(Flag(AtomicBool::new(false)));
                let waker = if park {
                    Waker::from(Arc::new(Unpark(std::thread::current())))
                } else {
                    Waker::from(Arc::clone(&flag))
                };
                line.wait();
                for _ in 0..per_task {
                    op(&waker, &flag);
                }
            })
        })
        .collect();
    let began = line.start();
    for thread in threads {
        thread.join().expect("worker");
    }
    began.elapsed()
}

/// Returns `Pending` once, waking itself, so a task suspends while it
/// holds the lock.
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Runs `total` locks spread over `tasks` tasks (`start::share`) on
/// this thread, polling only the tasks whose waker fired.
fn on_one_thread(mutex: &Mutex<u64>, tasks: usize, total: u64) -> Duration {
    let flags: Vec<_> = (0..tasks)
        .map(|_| Arc::new(Flag(AtomicBool::new(true))))
        .collect();
    let wakers: Vec<_> = flags.iter().map(|f| Waker::from(Arc::clone(f))).collect();
    let mut futures: Vec<Pin<Box<dyn Future<Output = ()> + '_>>> = (0..tasks)
        .map(|t| {
            let per_task = start::share(total, tasks, t);
            Box::pin(async move {
                for _ in 0..per_task {
                    let mut guard = mutex.lock().await;
                    YieldOnce(false).await;
                    *guard += 1;
                }
            }) as Pin<Box<dyn Future<Output = ()> + '_>>
        })
        .collect();
    let mut live = tasks;
    let began = Instant::now();
    while live > 0 {
        for i in 0..tasks {
            if !flags[i].0.swap(false, Ordering::Acquire) {
                continue;
            }
            if futures[i]
                .as_mut()
                .poll(&mut Context::from_waker(&wakers[i]))
                .is_ready()
            {
                live -= 1;
            }
        }
    }
    began.elapsed()
}

fn contended(c: &mut Criterion) {
    let mut group = c.benchmark_group("sync_contended");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(4));
    group.throughput(Throughput::Elements(1));
    for tasks in [2usize, 8, 32] {
        for (name, executor) in [
            ("nuclei_mutex_park", Executor::Park),
            ("nuclei_mutex_spin", Executor::Spin),
        ] {
            group.bench_with_input(BenchmarkId::new(name, tasks), &tasks, |b, &tasks| {
                b.iter_custom(|iters| {
                    let mutex = Arc::new(Mutex::new(0u64));
                    let park = matches!(executor, Executor::Park);
                    on_threads(
                        tasks,
                        iters,
                        move |waker, flag| {
                            *block_on(mutex.lock(), waker, flag, executor) += 1;
                        },
                        park,
                    )
                })
            });
        }
        group.bench_with_input(
            BenchmarkId::new("nuclei_mutex_one_thread", tasks),
            &tasks,
            |b, &tasks| {
                b.iter_custom(|iters| {
                    let mutex = Mutex::new(0u64);
                    on_one_thread(&mutex, tasks, iters)
                })
            },
        );
        group.bench_with_input(BenchmarkId::new("std_mutex", tasks), &tasks, |b, &tasks| {
            b.iter_custom(|iters| {
                let mutex = Arc::new(std::sync::Mutex::new(0u64));
                on_threads(
                    tasks,
                    iters,
                    move |_, _| {
                        *mutex.lock().expect("unpoisoned") += 1;
                    },
                    true,
                )
            })
        });
    }
    group.finish();
}

/// One contended shape of the fairness window: what each task does once.
#[derive(Clone, Copy)]
enum Shape {
    Park,
    Spin,
    Std,
}

/// Every task of a fairness run on its own thread takes the lock as often
/// as it can until `stop` is raised; returns each task's acquisitions.
fn window_on_threads(tasks: usize, window: Duration, shape: Shape) -> Vec<u64> {
    let ours = Arc::new(Mutex::new(0u64));
    let std_mutex = Arc::new(std::sync::Mutex::new(0u64));
    let stop = Arc::new(AtomicBool::new(false));
    let line = Arc::new(start::StartLine::new(tasks));
    let threads: Vec<_> = (0..tasks)
        .map(|_| {
            let (ours, std_mutex) = (Arc::clone(&ours), Arc::clone(&std_mutex));
            let (stop, line) = (Arc::clone(&stop), Arc::clone(&line));
            std::thread::spawn(move || {
                let flag = Arc::new(Flag(AtomicBool::new(false)));
                let waker = match shape {
                    Shape::Park | Shape::Std => {
                        Waker::from(Arc::new(Unpark(std::thread::current())))
                    }
                    Shape::Spin => Waker::from(Arc::clone(&flag)),
                };
                line.wait();
                let mut taken = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    match shape {
                        Shape::Park => {
                            *block_on(ours.lock(), &waker, &flag, Executor::Park) += 1;
                        }
                        Shape::Spin => {
                            *block_on(ours.lock(), &waker, &flag, Executor::Spin) += 1;
                        }
                        Shape::Std => *std_mutex.lock().expect("unpoisoned") += 1,
                    }
                    taken += 1;
                }
                taken
            })
        })
        .collect();
    line.start();
    std::thread::sleep(window);
    stop.store(true, Ordering::Relaxed);
    threads
        .into_iter()
        .map(|thread| thread.join().expect("worker"))
        .collect()
}

/// The one-thread shape of the fairness run: `tasks` tasks on this thread,
/// each holding the lock across one suspension, until the window closes.
fn window_on_one_thread(tasks: usize, window: Duration) -> Vec<u64> {
    let mutex = Mutex::new(0u64);
    let stop = core::cell::Cell::new(false);
    let counts: Vec<_> = (0..tasks).map(|_| core::cell::Cell::new(0u64)).collect();
    let flags: Vec<_> = (0..tasks)
        .map(|_| Arc::new(Flag(AtomicBool::new(true))))
        .collect();
    let wakers: Vec<_> = flags.iter().map(|f| Waker::from(Arc::clone(f))).collect();
    let mut futures: Vec<Pin<Box<dyn Future<Output = ()> + '_>>> = counts
        .iter()
        .map(|count| {
            let (mutex, stop) = (&mutex, &stop);
            Box::pin(async move {
                while !stop.get() {
                    let mut guard = mutex.lock().await;
                    YieldOnce(false).await;
                    *guard += 1;
                    count.set(count.get() + 1);
                }
            }) as Pin<Box<dyn Future<Output = ()> + '_>>
        })
        .collect();
    let began = Instant::now();
    let mut live = tasks;
    let mut done = vec![false; tasks];
    let mut polls = 0u32;
    while live > 0 {
        for i in 0..tasks {
            if done[i] || !flags[i].0.swap(false, Ordering::Acquire) {
                continue;
            }
            polls = polls.wrapping_add(1);
            if polls.is_multiple_of(1024) && began.elapsed() >= window {
                stop.set(true);
            }
            if futures[i]
                .as_mut()
                .poll(&mut Context::from_waker(&wakers[i]))
                .is_ready()
            {
                done[i] = true;
                live -= 1;
            }
        }
    }
    drop(futures);
    counts.iter().map(core::cell::Cell::get).collect()
}

/// `SYNC_FAIRNESS=<seconds>`: every contended shape for a fixed window,
/// printing the throughput beside each task's acquisitions.
/// `SYNC_FAIRNESS_ONLY=<name>/<tasks>` runs that one shape alone, for a
/// profiler to attach to.
fn fairness(window: Duration) {
    let only = std::env::var("SYNC_FAIRNESS_ONLY").ok();
    for tasks in [2usize, 8, 32] {
        for name in [
            "nuclei_mutex_park",
            "nuclei_mutex_spin",
            "nuclei_mutex_one_thread",
            "std_mutex",
        ] {
            if only
                .as_deref()
                .is_some_and(|only| only != format!("{name}/{tasks}"))
            {
                continue;
            }
            let counts = match name {
                "nuclei_mutex_park" => window_on_threads(tasks, window, Shape::Park),
                "nuclei_mutex_spin" => window_on_threads(tasks, window, Shape::Spin),
                "nuclei_mutex_one_thread" => window_on_one_thread(tasks, window),
                _ => window_on_threads(tasks, window, Shape::Std),
            };
            let total: u64 = counts.iter().sum();
            let (fewest, most) = (
                counts.iter().copied().min().unwrap_or(0),
                counts.iter().copied().max().unwrap_or(0),
            );
            println!(
                "fairness {name}/{tasks}: {:.2} M locks/s, per task min {fewest} max {most} \
                 (max/min {:.2})",
                total as f64 / window.as_secs_f64() / 1e6,
                most as f64 / fewest.max(1) as f64,
            );
        }
    }
}

criterion_group!(benches, uncontended, contended);

fn main() {
    if let Some(seconds) = std::env::var_os("SYNC_FAIRNESS") {
        let seconds: f64 = seconds
            .to_str()
            .and_then(|s| s.parse().ok())
            .expect("SYNC_FAIRNESS is a number of seconds");
        fairness(Duration::from_secs_f64(seconds));
        return;
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
