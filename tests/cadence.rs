//! How soon a busy shard's run loop looks at its IO: a receive a task
//! queued reaches the kernel and completes within a bound of time, however
//! the tasks around it behave.
//!
//! Each runtime has one worker, and its tasks keep the queue from ever being
//! empty, so only the loop's looks between tasks drive its IO. The bounds are
//! loose: far below the 61 task runs a look once waited for (about 0.9 s
//! with the 15 ms blockers, about 180 ms with the 3 ms spinners) and above
//! what the looks themselves take with room for a loaded machine.

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nuclei::runtime::{Builder, Runtime};

/// What the receive may take with the handoff on, runners blocking 15 ms.
const HANDOFF_WITHIN: Duration = Duration::from_millis(150);
/// The same with the handoff off: 16 runs of 3 ms to the look, with room.
const BOUND_WITHIN: Duration = Duration::from_millis(100);

fn runtime(handoff: Option<Duration>) -> Runtime {
    Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .handoff_after(handoff)
        .build()
        .expect("the runtime starts")
}

/// Spawns, on `rt`'s one worker, a task that connects to a listener and
/// receives one byte; returns the task and the peer's end of the connection,
/// once the receive is queued.
fn receiver(rt: &Runtime) -> (nuclei::Task<u8>, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let receiving = Arc::new(AtomicBool::new(false));
    let up = Arc::clone(&receiving);
    let reader = rt.spawn(async move {
        let stream = nuclei::Handle::<TcpStream>::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        up.store(true, Ordering::SeqCst);
        let n = stream.recv(&mut buf).await.unwrap();
        assert_eq!(n, 1, "the peer closed early");
        buf[0]
    });
    let (peer, _) = listener.accept().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !receiving.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the receive was never queued");
        std::thread::yield_now();
    }
    (reader, peer)
}

/// Writes the byte the receiver waits for and returns how long its receive
/// took to complete.
fn answered_in(rt: &Runtime, reader: nuclei::Task<u8>, mut peer: TcpStream) -> Duration {
    let start = Instant::now();
    std::io::Write::write_all(&mut peer, b"x").unwrap();
    assert_eq!(rt.block_on(reader), b'x');
    start.elapsed()
}

#[test]
fn a_receive_queued_before_runners_that_block_at_once_completes_without_waiting_for_the_run_count()
{
    let rt = runtime(Some(Duration::from_millis(10)));
    let (reader, peer) = receiver(&rt);
    // Two tasks that each block the thread past the handoff threshold, then
    // yield: every runner's first task blocks, so the shard is handed on
    // before any runner reaches the driver, unless a runner looks first.
    let blockers: Vec<_> = (0..2)
        .map(|_| {
            rt.spawn(async {
                for _ in 0..40 {
                    std::thread::sleep(Duration::from_millis(15));
                    nuclei::task::yield_now().await;
                }
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(30));
    let took = answered_in(&rt, reader, peer);
    assert!(took < HANDOFF_WITHIN, "the receive waited {took:?}");
    for blocker in blockers {
        rt.block_on(blocker);
    }
}

#[test]
fn a_receive_queued_behind_tasks_that_each_run_a_few_ms_completes_within_the_time_bound() {
    let rt = runtime(None);
    let (reader, peer) = receiver(&rt);
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
    let took = answered_in(&rt, reader, peer);
    stop.store(true, Ordering::Relaxed);
    for spinner in spinners {
        rt.block_on(spinner);
    }
    assert!(took < BOUND_WITHIN, "the receive waited {took:?}");
}
