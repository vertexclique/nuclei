//! A thread that sleeps holds nothing of kovan's: a shard that parks, a thread blocked in a
//! channel's `blocking_recv`, and every shard of an idle runtime leave their reservation first, so
//! a value any of them retired, or another thread retired after they slept, is freed while they
//! stay asleep. The test thread flushes to drain its own slot; no task runs on a shard after it
//! parks, and nothing wakes a blocked receiver until the value is freed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kovan::Atom;
use nuclei::runtime::Builder;
use nuclei::sync::mpsc;

/// How long a value gets to be freed: far past what a working runtime needs, and the time a
/// runtime that never frees it is watched for.
const SETTLE: Duration = Duration::from_secs(3);

/// Counts its drops.
struct Probe(Arc<AtomicUsize>);

impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Runs `retire` and flushes this thread, again and again, until `freed` counts a drop or the
/// deadline passes: a thread that parked after one round is quiet for the next.
fn freed_within(freed: &AtomicUsize, mut retire: impl FnMut()) -> bool {
    let deadline = Instant::now() + SETTLE;
    let mut pause = Duration::from_micros(100);
    while Instant::now() < deadline {
        retire();
        kovan::flush();
        if freed.load(Ordering::SeqCst) > 0 {
            return true;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(10));
    }
    false
}

/// One test at a time: the tests share kovan's process-wide reservations, so another test's
/// thread could hold what one waits for.
fn alone() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Stores a new probe into `atom`, which retires the one it held.
fn replace(atom: &Atom<Probe>, freed: &Arc<AtomicUsize>) {
    atom.store(Probe(Arc::clone(freed)));
}

#[test]
fn a_parked_shard_frees_what_its_task_retired() {
    let _alone = alone();
    let rt = Builder::new().worker_threads(1).build().unwrap();
    let freed = Arc::new(AtomicUsize::new(0));
    let atom = Arc::new(Atom::new(Probe(Arc::clone(&freed))));
    let (go, done) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let _task = {
        let (atom, go, done) = (Arc::clone(&atom), Arc::clone(&go), Arc::clone(&done));
        let later = Arc::new(AtomicUsize::new(0));
        rt.spawn(async move {
            while !go.load(Ordering::Acquire) {
                nuclei::task::yield_now().await;
            }
            replace(&atom, &later);
            done.store(true, Ordering::Release);
        })
    };
    // kovan places a small batch only past the reservations of the threads that hold one, so this
    // thread holds none (no pin, a flush) while the task retires and the shard parks after it.
    kovan::flush();
    go.store(true, Ordering::Release);
    let deadline = Instant::now() + SETTLE;
    while !done.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "the task never ran");
        std::thread::sleep(Duration::from_micros(200));
    }
    // The shard has nothing left to run: it parks, and runs nothing more.
    assert!(
        freed_within(&freed, || {}),
        "the value a task retired was held by the shard that parked after it"
    );
}

/// Runs `receiver` on a thread that receives one value, pins kovan, says so through the flag
/// it is given, and blocks on the next; `first` sends the value, `close` ends the channel. A
/// value retired round after round meanwhile must be freed with the receiver blocked.
fn blocked_receiver_frees(
    flavour: &str,
    receiver: impl FnOnce(&AtomicBool, &Atom<Probe>) + Send + 'static,
    first: impl FnOnce(),
    close: impl FnOnce(),
) {
    let freed = Arc::new(AtomicUsize::new(0));
    let atom = Arc::new(Atom::new(Probe(Arc::clone(&freed))));
    let got = Arc::new(AtomicBool::new(false));
    let thread = {
        let (got, atom) = (Arc::clone(&got), Arc::clone(&atom));
        std::thread::spawn(move || receiver(&got, &atom))
    };
    first();
    let deadline = Instant::now() + SETTLE;
    while !got.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the receiver never got its value"
        );
        std::thread::yield_now();
    }
    // The receiver pinned before this retire. The first probe, born before its pin, is the one
    // its reservation holds; the replacement is counted apart.
    let later = Arc::new(AtomicUsize::new(0));
    let mut once = Some(());
    let ok = freed_within(&freed, || {
        if once.take().is_some() {
            replace(&atom, &later);
        }
    });
    close();
    thread.join().unwrap();
    assert!(
        ok,
        "a value retired while a thread blocked in {flavour} blocking_recv was held by it"
    );
}

#[test]
fn a_thread_blocked_in_blocking_recv_holds_nothing() {
    let _alone = alone();
    let (tx, mut rx) = mpsc::channel::<u8>(4);
    let first = tx.clone();
    blocked_receiver_frees(
        "bounded",
        move |got, atom| {
            assert_eq!(rx.blocking_recv(), Some(1));
            // Pins kovan, as a thread that used a kovan structure has, and then blocks.
            drop(atom.load());
            got.store(true, Ordering::SeqCst);
            assert_eq!(rx.blocking_recv(), None);
        },
        move || first.blocking_send(1).unwrap(),
        move || drop(tx),
    );
}

#[test]
fn a_thread_blocked_in_unbounded_blocking_recv_holds_nothing() {
    let _alone = alone();
    let (tx, mut rx) = mpsc::unbounded_channel::<u8>();
    let first = tx.clone();
    blocked_receiver_frees(
        "unbounded",
        move |got, atom| {
            assert_eq!(rx.blocking_recv(), Some(1));
            // Pins kovan, as a thread that used a kovan structure has, and then blocks.
            drop(atom.load());
            got.store(true, Ordering::SeqCst);
            assert_eq!(rx.blocking_recv(), None);
        },
        move || first.send(1).unwrap(),
        move || drop(tx),
    );
}

/// Waits until every shard of `rt` is asleep: its time parked grows between two reads.
fn all_asleep(rt: &nuclei::Runtime) {
    let deadline = Instant::now() + SETTLE;
    loop {
        let before = rt.stats();
        std::thread::sleep(Duration::from_millis(5));
        let after = rt.stats();
        if before
            .shards
            .iter()
            .zip(&after.shards)
            .all(|(b, a)| a.parked > b.parked)
        {
            return;
        }
        assert!(Instant::now() < deadline, "the shards never all slept");
    }
}

#[test]
fn an_idle_runtime_holds_no_reservation_on_any_shard() {
    const SHARDS: usize = 4;
    let _alone = alone();
    let rt = Builder::new().worker_threads(SHARDS).build().unwrap();
    let freed = Arc::new(AtomicUsize::new(0));
    let atom = Arc::new(Atom::new(Probe(Arc::clone(&freed))));
    // Tasks live together spread over the shards, and each pins kovan on the shard it runs on.
    let release = Arc::new(AtomicBool::new(false));
    let tasks: Vec<_> = (0..SHARDS * 64)
        .map(|_| {
            let (atom, release) = (Arc::clone(&atom), Arc::clone(&release));
            rt.spawn(async move {
                drop(atom.load());
                while !release.load(Ordering::Acquire) {
                    nuclei::task::yield_now().await;
                }
            })
        })
        .collect();
    release.store(true, Ordering::Release);
    for task in tasks {
        rt.block_on(task);
    }
    // Every shard has pinned and goes idle; one retires a value after the others slept. kovan
    // places a small batch only past the reservations of the threads that hold one, so this
    // thread holds none while the task retires.
    all_asleep(&rt);
    let (go, done) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let _task = {
        let (atom, go, done) = (Arc::clone(&atom), Arc::clone(&go), Arc::clone(&done));
        let later = Arc::new(AtomicUsize::new(0));
        rt.spawn(async move {
            while !go.load(Ordering::Acquire) {
                nuclei::task::yield_now().await;
            }
            replace(&atom, &later);
            done.store(true, Ordering::Release);
        })
    };
    kovan::flush();
    go.store(true, Ordering::Release);
    let deadline = Instant::now() + SETTLE;
    while !done.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "the task never ran");
        std::thread::sleep(Duration::from_micros(200));
    }
    assert!(
        freed_within(&freed, || {}),
        "an idle shard held a reservation, and the value stayed live"
    );
}

#[test]
fn an_idle_blocking_thread_holds_nothing() {
    let _alone = alone();
    let rt = Builder::new()
        .worker_threads(1)
        .thread_keep_alive(Duration::from_secs(30))
        .build()
        .unwrap();
    let freed = Arc::new(AtomicUsize::new(0));
    let atom = Arc::new(Atom::new(Probe(Arc::clone(&freed))));
    let job = {
        let atom = Arc::clone(&atom);
        // Pins kovan on a pool thread, which then idles for its keep-alive.
        rt.spawn_blocking(move || drop(atom.load()))
    };
    rt.block_on(job);
    let later = Arc::new(AtomicUsize::new(0));
    let mut once = Some(());
    assert!(
        freed_within(&freed, || {
            if once.take().is_some() {
                replace(&atom, &later);
            }
        }),
        "a value retired while a blocking thread idled was held by it"
    );
}
