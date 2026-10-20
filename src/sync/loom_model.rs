//! Loom models that need the crate-private half of `nuclei::sync`.
//!
//! `tests/sync_loom.rs` drives the primitives through their public API,
//! which cannot see how often a drain passes. The model here wraps the
//! semaphore's [`Policy`] to count the passes of the drain one waiter runs,
//! so it lives inside the crate and `tests/sync_loom.rs` calls it through
//! `nuclei::sync::loom_model`.
//!
//! What it checks: a waiter that queues takes the drain role
//! in the same poll when nobody holds it. While it serves the queue, the
//! holder lets go and another caller keeps taking the permit and giving it
//! back. Every release that finds no nudge on its way asks for another
//! pass, so the waiter's drain must not be left without one: once the
//! waiter's own node is nudged, the marker of that nudge stays up until the
//! waiter looks, releases stop asking for passes, the drain ends, and the
//! waiter retries. The model asserts that no drain the waiter runs takes
//! more than [`MOST_PASSES`] passes and that the waiter is served.
//!
//! The calibration rebuilds the marker out of plain loom atomics the way it
//! was written before the fix (a nudge that finds its waiter already nudged
//! lowers the marker) and must fail: the drain passes once more for every
//! release.
//!
//! The next models drive the production `RawSemaphore` (a Mutex is its
//! one-permit case) through its own first attempt (`take_one`), spin before
//! queueing and spin on the waiter's node, each with a calibration that
//! rebuilds the one guard it checks the wrong way and must fail: the first
//! attempt must refuse a lock owed to a queued waiter, and a waiter that
//! stops spinning on its node must read the node's marks as it arms its
//! waker, since a release that nudged it while it spun called no waker.
//!
//! A further model covers the same hazard with nobody queued: a thread takes
//! the drain role, its pass finds the queue empty, and another thread keeps
//! letting a permit go and taking it back. A release that finds the role
//! taken leaves only its request to wake the front, and a drain whose pass
//! found nobody queued drops that request, so it runs one pass. Its
//! calibration writes the release the old way, raising the dirty bit, and
//! must fail.

use core::cell::Cell;
use core::task::Waker;
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};

use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::sync::{Arc, Notify};
use loom::thread;

use super::contend::{self, Contend, Lock, MAX_BYPASS};
use super::queue::{Arrivals, FIRST_BIT, HANDOFF, Policy, QUEUED, Step, WaitQueue};
use super::raw_semaphore::RawSemaphore;
use super::waiter::{Wait, WakeList};

/// The most passes one drain of the waiter may run in these models: the
/// pass it starts with, one more for the releases that found no nudge out
/// before its node was nudged, and one more for a release that had put its
/// permit back before the marker went up and asked for its pass after.
const MOST_PASSES: usize = 3;

/// How many times the other thread takes the permit back and lets it go:
/// with its first release, one release more than [`MOST_PASSES`] allows a
/// drain to answer with a pass of its own.
const BARGES: usize = 2;

/// Runs `model` under loom with at most `preemptions` preemptions per
/// schedule (`LOOM_MAX_PREEMPTIONS` overrides it), prints how large the
/// search was, and fails it if the search was implausibly small or never
/// reached the state the model is about (`witnessed`).
fn explore(
    name: &'static str,
    preemptions: usize,
    min_interleavings: usize,
    min_witnessed: usize,
    model: impl Fn(&StdAtomicUsize) + Sync + Send + 'static,
) {
    let runs = StdArc::new(StdAtomicUsize::new(0));
    let witnessed = StdArc::new(StdAtomicUsize::new(0));
    let counted = {
        let (runs, witnessed) = (StdArc::clone(&runs), StdArc::clone(&witnessed));
        move || {
            model(&witnessed);
            runs.fetch_add(1, StdOrdering::Relaxed);
        }
    };
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = builder.preemption_bound.or(Some(preemptions));
    builder.check(counted);
    let explored = runs.load(StdOrdering::Relaxed);
    let reached = witnessed.load(StdOrdering::Relaxed);
    println!("loom model {name}: {explored} interleavings explored, {reached} witnessed");
    assert!(
        explored >= min_interleavings,
        "{name} explored only {explored} interleavings, below the {min_interleavings} floor"
    );
    assert!(
        reached >= min_witnessed,
        "{name} reached its interesting state {reached} times, below the {min_witnessed} floor"
    );
}

/// The semaphore, with the passes of each drain this thread runs counted.
///
/// A poll can run several drains, one for each state change it has to
/// publish: queueing, the attempt that raises the owed bit, leaving the
/// queue. Each of those begins at a call this wrapper sees, so it starts a
/// new count there and keeps the largest.
struct Counted<'a> {
    sem: &'a RawSemaphore,
    passes: Cell<usize>,
    most: Cell<usize>,
}

impl Counted<'_> {
    /// What came before belongs to another drain: close its count.
    fn next_drain(&self) {
        self.most.set(self.most.get().max(self.passes.get()));
        self.passes.set(0);
    }

    /// The most passes any one drain ran since the last call.
    fn take_most(&self) -> usize {
        self.next_drain();
        self.most.replace(0)
    }
}

impl Policy for Counted<'_> {
    fn queue(&self) -> &WaitQueue {
        self.sem.queue()
    }

    fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, wake_front: bool) -> usize {
        self.passes.set(self.passes.get() + 1);
        self.sem.pass(arrivals, woken, wake_front)
    }

    /// As the provided method, with the drain it may run counted on its
    /// own.
    fn enqueue(&self, wait: &mut Wait, payload: usize, waker: &Waker) {
        wait.enqueue(self.queue(), payload, waker);
        self.next_drain();
        self.transition(|state| Step::Drain(state | QUEUED));
        self.next_drain();
    }
}

impl Lock for Counted<'_> {
    fn admit(&self, state: usize, need: usize) -> Option<usize> {
        self.sem.admit(state, need)
    }

    fn give_back(&self, need: usize) {
        self.sem.give_back(need);
    }

    /// As the provided method; what follows a successful attempt is the
    /// drain of leaving the queue, counted on its own.
    fn try_take(&self, need: usize) -> Result<(), usize> {
        self.next_drain();
        contend::take(self, need, false)
    }

    /// As the provided method, with the drain it may run counted on its
    /// own.
    fn take_or_owe(&self, wait: &Wait, need: usize) -> Result<(), usize> {
        self.next_drain();
        let attempt = self.owe_or_take(wait, |state| self.admit(state, need));
        self.next_drain();
        attempt
    }
}

/// Wakes the loom thread parked on it.
struct Unpark(Notify);

impl std::task::Wake for Unpark {
    fn wake(self: StdArc<Self>) {
        self.0.notify();
    }

    fn wake_by_ref(self: &StdArc<Self>) {
        self.0.notify();
    }
}

/// A waiter queues behind a holder and, when nobody holds the drain role,
/// serves the queue itself. The holder lets go, and takes the permit back
/// and lets it go [`BARGES`] more times. No drain of the waiter runs more
/// than [`MOST_PASSES`] passes, the waiter gets the permit, and no permit
/// is lost.
pub fn a_waiter_serving_the_queue_is_served_while_the_holder_barges() {
    explore(
        "a_waiter_serving_the_queue_is_served_while_the_holder_barges",
        3,
        100,
        1,
        |witnessed| {
            let sem = Arc::new(RawSemaphore::new(1));
            assert!(sem.try_acquire(1), "free at the start");
            let holder = {
                let sem = Arc::clone(&sem);
                thread::spawn(move || {
                    sem.release(1);
                    for _ in 0..BARGES {
                        // Lets the waiter run between two releases without
                        // spending one of the model's few preemptions, so
                        // each release can land in a pass of its own.
                        thread::yield_now();
                        if sem.try_acquire(1) {
                            sem.release(1);
                        }
                    }
                })
            };
            let counted = Counted {
                sem: &sem,
                passes: Cell::new(0),
                most: Cell::new(0),
            };
            let park = StdArc::new(Unpark(Notify::new()));
            let waker = Waker::from(StdArc::clone(&park));
            let mut contend = Contend::new();
            loop {
                let polled = contend.poll(&counted, 1, &waker);
                let passes = counted.take_most();
                if passes > 1 {
                    // A release asked for a pass while a drain worked.
                    witnessed.fetch_add(1, StdOrdering::Relaxed);
                }
                assert!(
                    passes <= MOST_PASSES,
                    "the drain ran {passes} passes for the waiter that holds it"
                );
                if polled.is_ready() {
                    break;
                }
                park.0.wait();
            }
            holder.join().expect("holder");
            sem.release(1);
            assert_eq!(sem.available(), 1, "a permit was lost or made up");
        },
    );
}

/// The marker of an outstanding nudge, written the way it was before the
/// drain bound was fixed: a pass that nudges a waiter already nudged lowers the marker
/// again. Every release after that finds the marker down and asks for a
/// pass, so the waiter that holds the drain role passes once more for
/// each: more than [`MOST_PASSES`] times.
pub fn calibration_a_marker_lowered_on_a_pending_nudge_keeps_the_drain_passing() {
    const DRAINING: usize = 1;
    const DIRTY: usize = 1 << 1;
    const NUDGE_OUT: usize = 1 << 2;
    const FREE: usize = 1 << 3;
    explore(
        "calibration_a_marker_lowered_on_a_pending_nudge_keeps_the_drain_passing",
        3,
        0,
        0,
        |_witnessed| {
            // The waiter has queued and holds the drain role; the other
            // thread holds the lock.
            let state = Arc::new(AtomicUsize::new(DRAINING));
            // The waiter's own node: whether it carries a nudge.
            let nudged = Arc::new(AtomicBool::new(false));
            let holder = {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    for _ in 0..=BARGES {
                        // A release: the permit goes back, and unless a
                        // nudge is out it asks for a pass. The role is
                        // taken, so asking is raising the dirty bit.
                        let before = state.fetch_or(FREE, Ordering::AcqRel);
                        if before & NUDGE_OUT == 0 {
                            state.fetch_or(DIRTY, Ordering::AcqRel);
                        }
                        // The permit is taken straight back.
                        state.fetch_and(!FREE, Ordering::AcqRel);
                        thread::yield_now();
                    }
                })
            };
            let mut passes = 0;
            loop {
                state.fetch_and(!DIRTY, Ordering::AcqRel);
                passes += 1;
                // The pass nudges the front waiter, which is this thread's
                // own node: marker up first, then the node.
                state.fetch_or(NUDGE_OUT, Ordering::AcqRel);
                let already = nudged.swap(true, Ordering::AcqRel);
                if already {
                    // The defect: the nudge found its waiter nudged already
                    // and is taken for "no nudge is out".
                    state.fetch_and(!NUDGE_OUT, Ordering::AcqRel);
                }
                let seen = state.load(Ordering::Acquire);
                if seen & DIRTY == 0
                    && state
                        .compare_exchange(
                            seen,
                            seen & !DRAINING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                {
                    break;
                }
            }
            holder.join().expect("holder");
            assert!(
                passes <= MOST_PASSES,
                "the drain ran {passes} passes for the waiter that holds it"
            );
        },
    );
}

/// How many times the other thread lets its permit go in the empty-queue
/// models.
const RELEASES: usize = 3;

/// A waiter gives up, and its withdrawal takes the drain role; the pass
/// finds nobody queued. Meanwhile the thread that holds the other permit
/// lets it go and takes it back [`RELEASES`] times. The drain runs one
/// pass, whatever those releases do, and no permit is lost.
pub fn a_drain_that_found_nobody_queued_ends_while_another_thread_releases() {
    explore(
        "a_drain_that_found_nobody_queued_ends_while_another_thread_releases",
        3,
        100,
        1,
        |witnessed| {
            let sem = Arc::new(RawSemaphore::new(2));
            assert!(sem.try_acquire(1), "the other thread's permit");
            let counted = Counted {
                sem: &sem,
                passes: Cell::new(0),
                most: Cell::new(0),
            };
            // A request for both permits queues behind the one that is out.
            let mut contend = Contend::new();
            assert!(
                contend.poll(&counted, 2, Waker::noop()).is_pending(),
                "one permit is out"
            );
            let other = {
                let sem = Arc::clone(&sem);
                thread::spawn(move || {
                    for _ in 0..RELEASES {
                        sem.release(1);
                        // Lets the drain run between two releases without
                        // spending one of the model's few preemptions.
                        thread::yield_now();
                        assert!(sem.try_acquire(1), "its permit is free again");
                    }
                })
            };
            // The waiter gives up while the other thread hammers.
            counted.take_most();
            contend.cancel(&counted, 2);
            let passes = counted.take_most();
            if passes == 1 {
                // This thread held the drain role over an empty queue.
                witnessed.fetch_add(1, StdOrdering::Relaxed);
            }
            assert!(
                passes <= 1,
                "the drain ran {passes} passes over a queue with nobody in it"
            );
            other.join().expect("other");
            sem.release(1);
            assert_eq!(sem.available(), 2, "a permit was lost or made up");
        },
    );
}

/// A release written the way it was before the drain was bounded: when it
/// finds the drain role taken, it raises the dirty bit. The thread that
/// holds the role over an empty queue then passes once more for each
/// release that lands before its exit.
pub fn calibration_a_release_that_dirties_keeps_an_empty_drain_passing() {
    const DRAINING: usize = 1;
    const DIRTY: usize = 1 << 1;
    const QUEUED: usize = 1 << 2;
    explore(
        "calibration_a_release_that_dirties_keeps_an_empty_drain_passing",
        3,
        0,
        0,
        |_witnessed| {
            // A thread holds the drain role; the waiter that raised QUEUED
            // is gone, so its pass will find nobody.
            let state = Arc::new(AtomicUsize::new(DRAINING | QUEUED));
            let other = {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    for _ in 0..RELEASES {
                        // A release that sees QUEUED up asks for a pass.
                        // The defect: with the role taken, asking is
                        // raising the dirty bit.
                        if state.load(Ordering::Acquire) & QUEUED != 0 {
                            state.fetch_or(DIRTY, Ordering::AcqRel);
                        }
                        thread::yield_now();
                    }
                })
            };
            let mut passes = 0;
            loop {
                state.fetch_and(!DIRTY, Ordering::AcqRel);
                // The pass: nobody is queued, so QUEUED may come down.
                passes += 1;
                let seen = state.load(Ordering::Acquire);
                if seen & DIRTY == 0
                    && state
                        .compare_exchange(
                            seen,
                            seen & !(DRAINING | QUEUED),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                {
                    break;
                }
            }
            other.join().expect("other");
            assert!(
                passes <= 1,
                "the drain ran {passes} passes over a queue with nobody in it"
            );
        },
    );
}

/// Drives a production [`Contend`] on `sem` to completion on this loom
/// thread, parking it between polls until its waker fires: a wakeup that
/// never comes parks it for good, which loom reports as a deadlock.
fn acquire_parking(sem: &RawSemaphore) {
    let park = StdArc::new(Unpark(Notify::new()));
    let waker = Waker::from(StdArc::clone(&park));
    let mut contend = Contend::new();
    while contend.poll(sem, 1, &waker).is_pending() {
        park.0.wait();
    }
}

/// The first attempt. A waiter queues behind the holder; the holder
/// lets go and at once tries to take the lock back with the Mutex's own
/// inline attempt, `MAX_BYPASS + 2` times, so the waiter loses its retries
/// until it is owed a handoff. Whenever the holder saw the handoff owed
/// before its attempt, the attempt must fail: the lock is the waiter's. The
/// waiter is served and no permit is lost.
pub fn the_first_attempt_refuses_a_lock_owed_to_a_waiter() {
    owed_handoff_model(
        "the_first_attempt_refuses_a_lock_owed_to_a_waiter",
        RawSemaphore::take_one,
    );
}

/// The first attempt written without its HANDOFF test: a swap from the
/// state loaded whenever a permit is free. Past the bound the holder takes
/// the lock that was owed to the waiter, and the model must fail.
pub fn calibration_a_first_attempt_that_ignores_handoff_barges_past_an_owed_waiter() {
    owed_handoff_model(
        "calibration_a_first_attempt_that_ignores_handoff_barges_past_an_owed_waiter",
        |sem| {
            const ONE: usize = 1 << FIRST_BIT;
            let state = &sem.queue().state;
            let seen = state.load(Ordering::Relaxed);
            if seen >= ONE {
                state
                    .compare_exchange(seen, seen - ONE, Ordering::Acquire, Ordering::Relaxed)
                    .map(drop)
            } else {
                Err(seen)
            }
        },
    );
}

/// SHRUNK to preemption bound 1: at bound 2 the four release-and-attempt
/// rounds did not finish in five minutes. Bound 1 explores 0.36 million
/// interleavings in about 25 s and reaches the owed state in 12 thousand of
/// them; every interleaving of the waiter's retries against the holder's
/// rounds is still explored, with one preemption on top.
fn owed_handoff_model(name: &'static str, attempt: fn(&RawSemaphore) -> Result<(), usize>) {
    explore(name, 1, 10, 1, move |witnessed| {
        let sem = Arc::new(RawSemaphore::new(1));
        assert!(sem.try_acquire(1), "free at the start");
        let waiter = {
            let sem = Arc::clone(&sem);
            thread::spawn(move || acquire_parking(&sem))
        };
        // Lets the waiter queue first, without spending a preemption.
        thread::yield_now();
        let mut held = true;
        for _ in 0..=MAX_BYPASS + 1 {
            if held {
                sem.release(1);
            }
            let owed = sem.queue().state.load(Ordering::Acquire) & HANDOFF != 0;
            held = attempt(&sem).is_ok();
            if owed {
                witnessed.fetch_add(1, StdOrdering::Relaxed);
                assert!(!held, "the holder took a lock owed to the queued waiter");
            }
            // The waiter retries now, against a lock the holder took back if
            // its attempt won: a loss it counts.
            thread::yield_now();
        }
        if held {
            sem.release(1);
        }
        waiter.join().expect("waiter");
        sem.release(1);
        assert_eq!(sem.available(), 1, "a permit was lost or made up");
    });
}

/// The spins. A waiter queues behind the holder; once it has queued,
/// the holder puts the lock's spin budget back to one round if the waiter's
/// spin before queueing used it up (as another waiter's won retry would), so
/// the waiter's next lost retry does spin on its node. The holder lets go and
/// takes the lock straight back, so the waiter's retry can lose; then it lets
/// go again, which can land while the waiter spins on its unarmed node (the
/// release calls no waker) or just after the spin's last read (only the look
/// sees it). The waiter is served all the same: a lost wakeup would park it
/// for good, which loom reports as a deadlock. The model counts the spins on
/// the node, the marks the spin saw, and the marks only the look saw, and
/// fails if any of the three never happened.
pub fn a_nudge_while_the_waiter_spins_is_never_lost() {
    node_spin_model(
        "a_nudge_while_the_waiter_spins_is_never_lost",
        [100, 10, 10],
    );
}

/// The same run with the look after a spin that runs out taken away
/// (`spin_probe::SKIP_LOOK`): the production `Wait::spin_park` then arms
/// the waker without reading the marks. A nudge that landed while the
/// waiter spun, or after its last read, is left unread beside a sleeper,
/// and the arming swap's check fails.
pub fn calibration_a_node_spin_that_skips_its_look_sleeps_beside_the_mark() {
    super::waiter::spin_probe::SKIP_LOOK.with(|skip| skip.set(true));
    node_spin_model(
        "calibration_a_node_spin_that_skips_its_look_sleeps_beside_the_mark",
        [0, 0, 0],
    );
}

/// The run of the two models above; `floors` are the least counts of node
/// spins, marks the spin saw, and marks only the look saw.
fn node_spin_model(name: &'static str, floors: [usize; 3]) {
    super::waiter::spin_probe::take();
    explore(name, 2, 10, 1, |witnessed| {
        let sem = Arc::new(RawSemaphore::new(1));
        assert!(sem.try_acquire(1), "free at the start");
        let waiter = {
            let sem = Arc::clone(&sem);
            thread::spawn(move || acquire_parking(&sem))
        };
        while sem.queue().state.load(Ordering::Acquire) & QUEUED == 0 {
            thread::yield_now();
        }
        if sem.queue().spin.rounds() == 0 {
            sem.queue().spin.won(0);
        }
        sem.release(1);
        if sem.take_one().is_ok() {
            // The holder took the lock back past the waiter.
            witnessed.fetch_add(1, StdOrdering::Relaxed);
            thread::yield_now();
            sem.release(1);
        }
        waiter.join().expect("waiter");
        sem.release(1);
        assert_eq!(sem.available(), 1, "a permit was lost or made up");
    });
    let [spins, seen, late] = super::waiter::spin_probe::take();
    println!(
        "loom model {name}: {spins} node spins, {seen} marks seen by the spin, {late} only by the look"
    );
    assert!(
        spins >= floors[0],
        "{name}: {spins} node spins, below the floor {}",
        floors[0]
    );
    assert!(
        seen >= floors[1],
        "{name}: {seen} marks seen by a spin, below the floor {}",
        floors[1]
    );
    assert!(
        late >= floors[2],
        "{name}: {late} marks seen only by the look, below the floor {}",
        floors[2]
    );
}
