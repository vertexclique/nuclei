//! A runtime's drop against its monitor's last pass (proofs/tla/teardown).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use super::Builder;

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
