//! How soon a busy shard's run loop looks at its IO: a receive a task
//! queued reaches the kernel and completes within a bound, however the tasks
//! around it behave.
//!
//! Each runtime has one worker, and its tasks keep the queue from ever being
//! empty, so only the loop's looks between tasks drive its IO. The bounds are
//! loose: far below what a look once waited for (every blocker running first,
//! or 61 runs: about 180 ms with the 3 ms spinners) and above what the looks
//! themselves take with room for a loaded machine.

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
/// What the receive may take with the handoff off: 16 runs of 3 ms to the
/// look, with room.
const BOUND_WITHIN: Duration = Duration::from_millis(100);

fn runtime(handoff: Option<Duration>) -> Runtime {
    Builder::new()
        .worker_threads(1)
        .max_worker_threads(BLOCKERS + 4)
        .pin_workers(false)
        .handoff_after(handoff)
        .build()
        .expect("the runtime starts")
}

/// Spawns, on `rt`'s one worker, a task that connects to a listener and
/// receives one byte, then reads `started`; returns the task and the peer's
/// end of the connection, once the receive is queued.
fn receiver(rt: &Runtime, started: &Arc<AtomicUsize>) -> (nuclei::Task<(u8, usize)>, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let receiving = Arc::new(AtomicBool::new(false));
    let up = Arc::clone(&receiving);
    let started = Arc::clone(started);
    let reader = rt.spawn(async move {
        let stream = nuclei::Handle::<TcpStream>::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        up.store(true, Ordering::SeqCst);
        let n = stream.recv(&mut buf).await.unwrap();
        assert_eq!(n, 1, "the peer closed early");
        (buf[0], started.load(Ordering::SeqCst))
    });
    let (peer, _) = listener.accept().unwrap();
    wait_for(
        || receiving.load(Ordering::SeqCst),
        "the receive was never queued",
    );
    (reader, peer)
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

/// Writes the byte the receiver waits for; returns how long its receive took
/// to complete and what it read of the count it was given.
fn answered_in(
    rt: &Runtime,
    reader: nuclei::Task<(u8, usize)>,
    mut peer: TcpStream,
) -> (Duration, usize) {
    let start = Instant::now();
    std::io::Write::write_all(&mut peer, b"x").unwrap();
    let (byte, seen) = rt.block_on(reader);
    assert_eq!(byte, b'x');
    (start.elapsed(), seen)
}

#[test]
fn a_receive_queued_before_runners_that_block_at_once_completes_without_waiting_for_the_run_count()
{
    let rt = runtime(Some(Duration::from_millis(10)));
    let started = Arc::new(AtomicUsize::new(0));
    let (reader, peer) = receiver(&rt, &started);
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
    let (_, seen) = answered_in(&rt, reader, peer);
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
fn a_receive_queued_behind_tasks_that_each_run_a_few_ms_completes_within_the_time_bound() {
    let rt = runtime(None);
    let (reader, peer) = receiver(&rt, &Arc::new(AtomicUsize::new(0)));
    // Two tasks that spin 3 ms and yield: the queue is never empty, and no
    // run is long enough for the shard to be handed on (it is off), so only
    // the time bound brings a look before 61 runs.
    let stop = Arc::new(AtomicBool::new(false));
    let spinners: Vec<_> = (0..2)
        .map(|_| {
            let stop = Arc::clone(&stop);
            rt.spawn(async move {
                while !stop.load(Ordering::Relaxed) {
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
    let (took, _) = answered_in(&rt, reader, peer);
    stop.store(true, Ordering::Relaxed);
    for spinner in spinners {
        rt.block_on(spinner);
    }
    assert!(took < BOUND_WITHIN, "the receive waited {took:?}");
}
