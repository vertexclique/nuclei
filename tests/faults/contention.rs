//! Tasks whose first polls block across handoffs, then return pending or ready, while other
//! threads wake and drop them: each ends exactly once, whoever edits the shards' task lists, and
//! the runtime's drop leaves none live.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// The case ends well within this on any host; the longest wait is a few blocked polls.
const BOUND: Duration = Duration::from_secs(10);

const TASKS: usize = 400;
/// Tasks that return from their blocked poll at the same moment.
const GROUP: usize = 8;

/// How many times each task's future was dropped.
struct Ends(Vec<AtomicUsize>);

struct Probe {
    ends: Arc<Ends>,
    id: usize,
    polls: usize,
    ready_at: usize,
    wakers: Arc<Mutex<Vec<Waker>>>,
    /// When the blocked first poll returns: a group of tasks returns together.
    release: Instant,
}

impl Future for Probe {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.polls == 0 {
            // Blocks past the handoff threshold, so the monitor takes the shard meanwhile.
            while Instant::now() < self.release {
                std::hint::spin_loop();
            }
        }
        self.polls += 1;
        if self.polls >= self.ready_at {
            return Poll::Ready(());
        }
        self.wakers.lock().unwrap().push(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.ends.0[self.id].fetch_add(1, Ordering::SeqCst);
    }
}

/// A short task that is listed at its first pending poll and unlisted as it ends: a stream of
/// list edits by the shards' owners while blocked polls come back.
struct Churn {
    ended: Arc<AtomicUsize>,
    polled: bool,
    wakers: Arc<Mutex<Vec<Waker>>>,
}

impl Future for Churn {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.polled {
            return Poll::Ready(());
        }
        self.polled = true;
        self.wakers.lock().unwrap().push(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Churn {
    fn drop(&mut self) {
        self.ended.fetch_add(1, Ordering::SeqCst);
    }
}

/// Every task ends once with takes forced all along: ready at the first poll, ready after wakes
/// from other threads, or pending until cancelled from another thread or by the runtime's drop.
#[test]
fn tasks_taken_across_handoffs_end_once_each() {
    let started = Instant::now();
    let rt = Arc::new(
        nuclei::Runtime::builder()
            .worker_threads(4)
            .pin_workers(false)
            .handoff_after(Some(Duration::from_millis(5)))
            .build()
            .unwrap(),
    );
    let ends = Arc::new(Ends((0..TASKS).map(|_| AtomicUsize::new(0)).collect()));
    let wakers = Arc::new(Mutex::new(Vec::new()));
    let base = Instant::now();
    let mut cancelled = Vec::new();
    let mut finishing = 0;
    for id in 0..TASKS {
        let ready_at = match id % 3 {
            0 => 1,
            1 => 3,
            _ => usize::MAX,
        };
        let task = rt.spawn(Probe {
            ends: Arc::clone(&ends),
            id,
            polls: 0,
            ready_at,
            wakers: Arc::clone(&wakers),
            release: base + Duration::from_millis(12 + (id / GROUP) as u64 * 4),
        });
        match id % 3 {
            // Cancelled from another thread while the runtime runs, idle or woken.
            2 if id % 2 == 0 => cancelled.push(task),
            1 if id % 4 == 1 => {
                finishing += 1;
                cancelled.push(task);
            }
            // Left pending for the runtime's drop to end.
            2 => task.detach(),
            _ => {
                finishing += 1;
                task.detach();
            }
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let waker = {
        let (wakers, stop) = (Arc::clone(&wakers), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let batch = std::mem::take(&mut *wakers.lock().unwrap());
                for waker in batch {
                    waker.wake();
                }
                std::thread::yield_now();
            }
        })
    };
    let (spawned, churned) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let churn = {
        let (rt, wakers, stop) = (Arc::clone(&rt), Arc::clone(&wakers), Arc::clone(&stop));
        let (spawned, churned) = (Arc::clone(&spawned), Arc::clone(&churned));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                spawned.fetch_add(1, Ordering::SeqCst);
                rt.spawn(Churn {
                    ended: Arc::clone(&churned),
                    polled: false,
                    wakers: Arc::clone(&wakers),
                })
                .detach();
                if spawned.load(Ordering::SeqCst) % 64 == 0 {
                    std::thread::yield_now();
                }
            }
        })
    };
    let dropper = std::thread::spawn(move || {
        for task in cancelled {
            drop(task);
            std::thread::yield_now();
        }
    });
    let deadline = Instant::now() + BOUND;
    let finished = || {
        (0..TASKS)
            .filter(|id| id % 3 != 2)
            .all(|id| ends.0[id].load(Ordering::SeqCst) == 1)
    };
    while !finished() {
        assert!(
            Instant::now() < deadline,
            "{finishing} tasks did not end in {BOUND:?}"
        );
        std::thread::yield_now();
    }
    dropper.join().unwrap();
    stop.store(true, Ordering::SeqCst);
    churn.join().unwrap();
    waker.join().unwrap();
    wakers.lock().unwrap().clear();
    let taken: u64 = rt.stats().shards.iter().map(|shard| shard.handoffs).sum();
    eprintln!("{taken} handoffs");
    assert!(
        taken > 0,
        "no shard was taken, so no edit crossed a handoff"
    );
    let Ok(rt) = Arc::try_unwrap(rt) else {
        panic!("a thread kept the runtime");
    };
    drop(rt);
    assert_eq!(
        churned.load(Ordering::SeqCst),
        spawned.load(Ordering::SeqCst),
        "a churn task ended not once"
    );
    for (id, ends) in ends.0.iter().enumerate() {
        assert_eq!(ends.load(Ordering::SeqCst), 1, "task {id} ended not once");
    }
    assert!(
        started.elapsed() < BOUND,
        "the case took {:?}",
        started.elapsed()
    );
}
