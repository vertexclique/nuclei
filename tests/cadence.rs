//! How soon a busy shard's run loop looks at its IO: a receive a task
//! queued reaches the kernel and completes within a bound, however the tasks
//! around it behave.
//!
//! Each runtime has one worker, and its tasks keep the queue from ever being
//! empty, so only the loop's looks between tasks drive its IO. Each bound
//! counts the tasks that ran before the receive completed, never the time it
//! took, which a loaded machine stretches: far below what a look once waited
//! for (every blocker running first, or 61 runs) and above what the loop's own
//! rule allows.

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nuclei::runtime::{Builder, Runtime};

/// Tasks that each block their thread once, every one of them taking over a
/// runner: fewer than the 16 runs after which the time bound looks.
const BLOCKERS: usize = 12;
/// How long each blocker holds its thread: past what the handoff watch
/// resolves on every platform (on Windows its ticks take the system timer's
/// 15.6 ms, four of them for the 10 ms threshold).
const BLOCK: Duration = Duration::from_millis(100);
/// The blockers that may start before the receive completes with the handoff
/// on: the one running when the byte is sent and the runners the watch takes
/// the shard to before a look, with room.
const STARTED_WITHIN: usize = BLOCKERS / 2;
/// The spinners' runs the receive may wait behind with the handoff off, once
/// its byte is in the socket: the 16 runs after which the time bound looks,
/// once more for a look the coarse clock put off, and the two spinners queued
/// ahead of the receive. The count alone looks after 61.
const RUNS_WITHIN: usize = 2 * 16 + 2;

fn runtime(handoff: Option<Duration>) -> Runtime {
    Builder::new()
        .worker_threads(1)
        .max_worker_threads(BLOCKERS + 4)
        .pin_workers(false)
        .handoff_after(handoff)
        .build()
        .expect("the runtime starts")
}

/// A task on `rt`'s one worker that receives one byte and then reads its
/// count, the peer's end of its connection, a copy of its own end to look at
/// the socket with, and whether the receive completed.
struct Receiver {
    task: nuclei::Task<(u8, usize)>,
    peer: TcpStream,
    look: TcpStream,
    done: Arc<AtomicBool>,
}

/// Spawns a [`Receiver`] that reads `count` once its byte came; returns once
/// the receive is queued.
fn receiver(rt: &Runtime, count: &Arc<AtomicUsize>) -> Receiver {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mine, _) = listener.accept().unwrap();
    let look = mine.try_clone().unwrap();
    look.set_nonblocking(true).unwrap();
    let receiving = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let (up, finished, count) = (Arc::clone(&receiving), Arc::clone(&done), Arc::clone(count));
    let task = rt.spawn(async move {
        let stream = nuclei::Handle::new(mine).unwrap();
        let mut buf = [0u8; 1];
        up.store(true, Ordering::SeqCst);
        let n = stream.recv(&mut buf).await.unwrap();
        let seen = count.load(Ordering::SeqCst);
        finished.store(true, Ordering::SeqCst);
        assert_eq!(n, 1, "the peer closed early");
        (buf[0], seen)
    });
    wait_for(
        || receiving.load(Ordering::SeqCst),
        "the receive was never queued",
    );
    Receiver {
        task,
        peer,
        look,
        done,
    }
}

/// Waits, with a bounded backoff, until `done`.
fn wait_for(done: impl Fn() -> bool, never: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pause = Duration::from_micros(10);
    while !done() {
        assert!(Instant::now() < deadline, "{never}");
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(1));
    }
}

/// Writes the byte the receiver waits for; returns what the receiver read of
/// the count it was given, and the count once the byte was in its socket (or,
/// if the receive took it first, once the receive completed).
fn answered(rt: &Runtime, mut receiver: Receiver, count: &AtomicUsize) -> (usize, usize) {
    std::io::Write::write_all(&mut receiver.peer, b"x").unwrap();
    wait_for(
        || {
            matches!(receiver.look.peek(&mut [0u8; 1]), Ok(1))
                || receiver.done.load(Ordering::SeqCst)
        },
        "the byte never reached the socket",
    );
    let arrived = count.load(Ordering::SeqCst);
    let (byte, seen) = rt.block_on(receiver.task);
    assert_eq!(byte, b'x');
    (seen, arrived)
}

#[test]
fn a_receive_queued_before_runners_that_block_at_once_completes_without_waiting_for_the_run_count()
{
    let rt = runtime(Some(Duration::from_millis(10)));
    let started = Arc::new(AtomicUsize::new(0));
    let reader = receiver(&rt, &started);
    // Tasks that each block the thread past the handoff threshold: every
    // runner's first task blocks, so the shard is handed on before any runner
    // reaches the driver, unless a runner looks first. Without that look the
    // receive waits until every blocker has started and the queue runs dry.
    let blockers: Vec<_> = (0..BLOCKERS)
        .map(|_| {
            let started = Arc::clone(&started);
            rt.spawn(async move {
                started.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(BLOCK);
            })
        })
        .collect();
    wait_for(|| started.load(Ordering::SeqCst) > 0, "no blocker ran");
    let (seen, _) = answered(&rt, reader, &started);
    for blocker in blockers {
        rt.block_on(blocker);
    }
    let handoffs: u64 = nuclei::stats().shards.iter().map(|s| s.handoffs).sum();
    assert!(handoffs > 0, "no runner took the shard over");
    assert!(
        seen < STARTED_WITHIN,
        "{seen} of {BLOCKERS} blockers started before the receive completed"
    );
}

#[test]
fn a_receive_queued_behind_tasks_that_each_run_a_few_ms_completes_within_the_run_bound() {
    let rt = runtime(None);
    let runs = Arc::new(AtomicUsize::new(0));
    let reader = receiver(&rt, &runs);
    // Two tasks that spin 3 ms and yield: the queue is never empty, and no
    // run is long enough for the shard to be handed on (it is off), so only
    // the time bound brings a look before 61 runs.
    let stop = Arc::new(AtomicBool::new(false));
    let spinners: Vec<_> = (0..2)
        .map(|_| {
            let (stop, runs) = (Arc::clone(&stop), Arc::clone(&runs));
            rt.spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let until = Instant::now() + Duration::from_millis(3);
                    while Instant::now() < until {
                        std::hint::spin_loop();
                    }
                    nuclei::task::yield_now().await;
                }
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(5));
    let (seen, arrived) = answered(&rt, reader, &runs);
    stop.store(true, Ordering::Relaxed);
    for spinner in spinners {
        rt.block_on(spinner);
    }
    let waited = seen.saturating_sub(arrived);
    assert!(
        waited <= RUNS_WITHIN,
        "the receive waited behind {waited} runs"
    );
}
