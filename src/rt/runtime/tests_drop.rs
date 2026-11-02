//! A runtime's drop and timed shutdown against its monitor's last pass, a
//! worker still running, and a worker whose thread cannot start
//! (proofs/tla/teardown, proofs/tla/placement/Publish.tla).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use super::Builder;
use crate::rt::workers::{THREAD_FAILS, Workers};

/// Waits for `done` with a deadline of `within`; whether it came.
fn within(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    let mut pause = Duration::from_micros(50);
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(5));
    }
    true
}

/// A block a task holds its thread at until the test opens it, or ten
/// seconds passed.
#[derive(Default)]
struct Gate(AtomicBool);

impl Gate {
    fn hold(&self) {
        within(Duration::from_secs(10), || self.0.load(Ordering::SeqCst));
    }

    fn open(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// `shutdown_timeout` received one exit message per worker not finished, from
/// whichever thread sent it: an exit left from a worker stopped before (or the
/// monitor's, or a detached thread's) counted for a worker still running its
/// task, and the shutdown returned while that worker ran on. It waits for each
/// worker to end, messages only waking it.
#[test]
fn a_timed_shutdown_waits_for_a_running_worker_past_an_exit_left_from_before() {
    let rt = Builder::new()
        .worker_threads(1)
        .max_worker_threads(2)
        .pin_workers(false)
        .handoff_after(None)
        .build()
        .unwrap();
    assert_eq!(Workers::start(rt.inner(), 1).unwrap(), 1);
    assert!(rt.inner().workers.stop_one(), "the second worker stops");
    assert!(
        within(Duration::from_secs(10), || !rt.exits.is_empty()),
        "the stopped worker never sent its exit"
    );
    let (started, began) = std::sync::mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let (inner, ran) = (Arc::clone(rt.inner()), Arc::clone(&done));
    rt.spawn(async move {
        started.send(()).unwrap();
        // Still running once the shutdown began.
        within(Duration::from_secs(10), || inner.lingering().is_closing());
        std::thread::sleep(Duration::from_millis(50));
        ran.store(true, Ordering::SeqCst);
    })
    .detach();
    began.recv_timeout(Duration::from_secs(10)).unwrap();
    rt.shutdown_timeout(Duration::from_secs(20));
    assert!(
        done.load(Ordering::SeqCst),
        "the timed shutdown returned while a worker still ran its task"
    );
}

/// A worker's shard was published to placement before its thread was asked
/// for: a spawn in between queued its task there, the thread did not start,
/// and the shard was stopped and dropped with the task. The shard is published
/// only once its thread took its run, so the spawn goes where tasks run.
#[test]
fn a_worker_whose_thread_cannot_start_drops_no_task_placed_meanwhile() {
    let rt = Builder::new()
        .worker_threads(0)
        .max_worker_threads(1)
        .pin_workers(false)
        .build()
        .unwrap();
    let handle = rt.handle();
    let placed = Rc::new(RefCell::new(None));
    let slot = Rc::clone(&placed);
    THREAD_FAILS.with(|fails| {
        *fails.borrow_mut() = Some(Box::new(move || {
            *slot.borrow_mut() = Some(handle.spawn(async { 5u32 }));
        }));
    });
    assert!(
        Workers::start(rt.inner(), 1).is_err(),
        "the thread start did not fail"
    );
    let task = placed
        .borrow_mut()
        .take()
        .expect("no spawn ran as the start failed");
    let out = rt.block_on(crate::time::timeout(Duration::from_secs(10), task.join()));
    assert!(
        matches!(out, Ok(Ok(5))),
        "the task placed as the thread start failed was dropped unrun"
    );
}

/// What a round of the late take saw.
enum Round {
    /// The monitor took the second worker's shard after the close: the drop's
    /// thread, and the gates the two blocked polls hold at.
    Late(std::thread::JoinHandle<()>, [Arc<Gate>; 2]),
    /// It did not (its streak was a pass off the first's): the round proves
    /// nothing.
    Missed,
}

/// One round: two workers blocked in a poll at one moment, so the monitor's
/// pass that takes the first one's shard reaches the second's fourth equal
/// read too; the long-poll hook, told of the first after its take, holds that
/// pass until the drop took its list of threads (or a second went by, as it
/// does once the drop joins the monitor first). The pass then takes the
/// second worker's shard, after the close.
fn late_take_round() -> Round {
    let target = Arc::new(Mutex::new(None));
    let held = Arc::new(Gate::default());
    let in_hook = Arc::new(AtomicBool::new(false));
    let (t, h, entered) = (Arc::clone(&target), Arc::clone(&held), Arc::clone(&in_hook));
    let rt = Builder::new()
        .worker_threads(2)
        .pin_workers(false)
        .handoff_after(Some(Duration::from_millis(20)))
        .on_long_poll(move |meta| {
            if *t.lock().unwrap() == Some(meta.id()) {
                entered.store(true, Ordering::SeqCst);
                h.hold();
            }
        })
        .build()
        .unwrap();
    let handle = rt.handle();
    assert!(within(Duration::from_secs(10), || handle.workers() == 2));
    let gates = [Arc::new(Gate::default()), Arc::new(Gate::default())];
    let at_once = Arc::new(Barrier::new(2));
    let tasks: Vec<_> = (0..2)
        .map(|k| {
            let (gate, at_once) = (Arc::clone(&gates[k]), Arc::clone(&at_once));
            handle
                .spawn_on_worker(k, async move {
                    at_once.wait();
                    gate.hold();
                })
                .unwrap()
        })
        .collect();
    *target.lock().unwrap() = Some(tasks[0].id());
    tasks.into_iter().for_each(crate::Task::detach);
    assert!(
        within(Duration::from_secs(10), || in_hook.load(Ordering::SeqCst)),
        "the first worker's blocked poll was never told"
    );
    let inner = Arc::clone(rt.inner());
    let second = inner
        .workers
        .monitor_first_life(1)
        .expect("the second place is watched");
    // Taken before the close, the second worker's shard proves nothing.
    let early = second.is_detached();
    let dropper = std::thread::spawn(move || drop(rt));
    // The list is taken (before the fix), or the drop waits for the monitor.
    within(Duration::from_secs(1), || {
        inner.workers.threads_listed() == 0
    });
    held.open();
    if !early && within(Duration::from_secs(1), || second.is_detached()) {
        return Round::Late(dropper, gates);
    }
    gates.iter().for_each(|gate| gate.open());
    dropper.join().unwrap();
    Round::Missed
}

/// The monitor's loop looks at the close at its head only, so a pass under
/// way as a drop closes it may still take a shard and start a runner for it:
/// the drop took its list of threads first, waited on the thread blocked in
/// the taken poll (which may never return), and never joined the runner. The
/// drop joins the monitor first, then takes its list: the late take's thread
/// is detached by then and left, and its runner joined.
#[test]
fn a_drop_never_waits_on_a_thread_its_monitor_takes_as_it_closes() {
    for _ in 0..10 {
        let Round::Late(dropper, gates) = late_take_round() else {
            continue;
        };
        let returned = within(Duration::from_secs(3), || dropper.is_finished());
        gates.iter().for_each(|gate| gate.open());
        assert!(
            returned,
            "the drop waits on a thread blocked in a poll whose shard was taken"
        );
        dropper.join().unwrap();
        return;
    }
    panic!("no round took a shard after the close");
}

/// Drops `rt` on a thread of its own, after `pause` set what the drop runs
/// at one moment of its teardown (`after_take`); the dropping thread.
fn drop_with_pause(
    rt: super::Runtime,
    after_take: bool,
    pause: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        super::TEARDOWN_PAUSE.with(|slot| *slot.borrow_mut() = Some((after_take, Box::new(pause))));
        drop(rt);
    })
}

/// A worker started as its runtime dropped, once the drop stopped every shard
/// and before it took its list of threads: the drop took the new worker's
/// thread and waited on it, its shard never stopped, for good. The drop stops
/// every shard again once it took its list, and the new worker's shard was in
/// its slot by then.
#[test]
fn a_drop_never_waits_for_good_on_a_worker_started_as_it_stopped_the_shards() {
    let rt = Builder::new()
        .worker_threads(1)
        .max_worker_threads(2)
        .pin_workers(false)
        .build()
        .unwrap();
    let inner = Arc::clone(rt.inner());
    let starter = Arc::clone(&inner);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let dropper = drop_with_pause(rt, false, move || {
        started_tx.send(Workers::start(&starter, 1).ok()).unwrap();
    });
    let started = started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the drop never came to its take");
    let returned = within(Duration::from_secs(3), || dropper.is_finished());
    if !returned {
        // Lets the drop go before the test fails.
        inner.workers.stop_all();
    }
    assert!(
        returned,
        "the drop waits for good on a worker started as it stopped the shards"
    );
    dropper.join().unwrap();
    assert_eq!(
        started,
        Some(1),
        "the start in the drop's window started no worker"
    );
}

/// A worker started as its runtime dropped, once the drop took its list of
/// threads: the thread went on a list nobody took again, its shard published
/// after the drop's stop, and the drop returned while that worker ran on,
/// never stopped or joined. A start once the list is taken starts nothing.
#[test]
fn a_worker_start_after_a_drop_took_its_threads_starts_nothing() {
    let rt = Builder::new()
        .worker_threads(1)
        .max_worker_threads(2)
        .pin_workers(false)
        .build()
        .unwrap();
    let inner = Arc::clone(rt.inner());
    let starter = Arc::clone(&inner);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let dropper = drop_with_pause(rt, true, move || {
        started_tx.send(Workers::start(&starter, 1).ok()).unwrap();
    });
    let started = started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the drop never took its list");
    assert!(
        within(Duration::from_secs(10), || dropper.is_finished()),
        "the drop never returned"
    );
    dropper.join().unwrap();
    let listed = inner.workers.threads_listed();
    // Lets a worker the start left behind go before the test fails.
    inner.workers.stop_all();
    assert_eq!(
        started,
        Some(0),
        "a start after the drop took its threads started a worker"
    );
    assert_eq!(
        listed, 0,
        "a thread went on the list after the drop took it"
    );
}
