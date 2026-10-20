//! Loom models for `nuclei::sync`.
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --release --test sync_loom
//! RUSTFLAGS="--cfg loom" cargo test --release --test sync_loom -- --ignored
//! ```
//!
//! The second line runs the `#[ignore]`d twins: the models whose three
//! threads all contend for one lock, at a deeper preemption bound (see
//! `contended!`).
//!
//! Under `--cfg loom` every atomic and protocol cell in the primitives is
//! loom's instrumented one, so these models check the wait protocol itself
//! (the arrival stack, the drain role, the waiter state machine, the
//! waker slot and the per-primitive state words) under every interleaving
//! loom reaches within the preemption bound. What each model proves:
//!
//! - **Mutual exclusion**: the guarded value is a `loom::cell::UnsafeCell`,
//!   so two holders at once fail the model as a data race.
//! - **No lost wakeup**: every wait goes through `loom::future::block_on`,
//!   which parks the loom thread until its waker fires; a wakeup that is
//!   never delivered leaves every thread parked and loom reports the
//!   deadlock.
//! - **Barging with bounded bypass**: `MAX_BYPASS` is 2 under loom, so a
//!   waiter that a barging thread keeps passing over reaches its handoff
//!   within a few operations, and the model covers the owed state, the
//!   handoff and the return to barging.
//! - **Cancellation at every await point**: a future polled once and then
//!   dropped races the release that may hand it the lock or nudge it;
//!   whatever it was given must reach the next waiter.
//! - **No access after free**: a waiter node handed back to the allocator
//!   counts as written (`Waiter::retire`), so a party that touches a node
//!   after letting go of its reference fails every model as a race.
//! - **Reentrancy depth**: an owner nests and unwinds while another owner
//!   waits.
//! - **The upgradable read**: it shares with readers, upgrades while they
//!   come and go, and excludes writers. A dropped upgrade never lets a
//!   writer in beside the next holder of the upgradable read; a calibration
//!   rebuilds the pass that took the write for a dropped upgrade's node and
//!   must fail.
//!
//! - **A drain that ends**: a waiter that serves the queue itself while the
//!   holder lets go and barges back runs a bounded number of passes and is
//!   served; and a thread that holds the drain role over an empty queue
//!   runs one pass, whatever another thread releases. Those models count
//!   passes, which the public API cannot see, so they live in
//!   `src/sync/loom_model.rs`, each with a calibration that writes its step
//!   the wrong way and must fail.
//!
//! - **The spins**: under loom a spin has one round. Every model here
//!   runs the spin before queueing; the spin on the waiter's node is
//!   reached only where a model makes sure of it, because a spin that runs
//!   out halves the budget to zero rounds. Two models check the spins
//!   directly on the production semaphore: the Mutex's inline first attempt
//!   refuses a lock owed to a queued waiter, and a nudge that lands while
//!   the waiter spins on its node, calling no waker, or just after the
//!   spin's last read, is never lost; the second counts the spins on the
//!   node and the marks each way and fails if any count is below its floor.
//!   Each has a calibration that takes the guard away and must fail.
//!
//! - **A semaphore that closes**: `close` races a queued acquire, a release
//!   with an acquire, and a cancelled acquire. Every wait ends (a wait left
//!   parked is a deadlock), none gets a permit the close had refused, and the
//!   permits free at the end are the permits there were: none leaks, none is
//!   counted twice.
//! - **A write that becomes a read**: the downgrade lets no writer in
//!   between (the guarded value is a `loom::cell::UnsafeCell`, so a writer
//!   inside beside the read is a race), and wakes the readers queued behind
//!   the write to share it; a bounded lock never has more readers inside than
//!   it was built for. The calibration of the downgrade releases the write
//!   and reads again, and must lose the value to a writer.
//! - **Notify's contract**: a `Notified` made before `notify_waiters` ends
//!   wherever its first poll lands, `notify_one` with nobody waiting stores
//!   exactly one permit, `notify_last` is never lost, and an enabled future
//!   receives a `notify_one` sent before its first poll.
//! - **The cells**: `set` racing an initializer leaves one value, and a
//!   `SetOnce` waited on is woken by the one `set` that wins.
//!
//! The waiter-node free list is a pass-through to the allocator under loom
//! (see `Pool`), so node recycling itself is covered by miri and the unit
//! tests rather than here. The calibration models deliberately expect
//! something the protocol does not guarantee and must fail; without them
//! a search that never reached the bad interleaving would pass for the
//! same reason a broken one does.

#![cfg(loom)]

use std::future::Future;

use std::pin::Pin;

use std::sync::atomic::{AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};

use std::task::{Context, Poll};

use loom::cell::UnsafeCell;

use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use loom::sync::{Arc, Notify as Park};

use loom::thread;

use nuclei::sync::{
    Barrier, Event, Latch, MAX_BYPASS, Mutex, Notify, OnceCell, Owner, ReentrantMutex,
    ReentrantRwLock, RwLock, RwLockWriteGuard, Semaphore, SetOnce, TryAcquireError,
};

/// Runs `model` under loom with at most `preemptions` preemptions per
/// schedule (`LOOM_MAX_PREEMPTIONS` overrides it) and fails it if the
/// search was implausibly small. Two-thread models take three
/// preemptions and three-thread models two, except the ones
/// `contended!` declares and the ones whose comment says they were
/// shrunk: every model is sized to finish in under a minute.
fn explore(
    name: &str,
    preemptions: usize,
    min_interleavings: usize,
    model: impl Fn() + Sync + Send + 'static,
) {
    let runs = std::sync::Arc::new(StdAtomicUsize::new(0));
    let counted = {
        let runs = std::sync::Arc::clone(&runs);
        move || {
            model();
            runs.fetch_add(1, StdOrdering::Relaxed);
        }
    };
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = builder.preemption_bound.or(Some(preemptions));
    builder.check(counted);
    let explored = runs.load(StdOrdering::Relaxed);
    println!("loom model {name}: {explored} interleavings explored");
    assert!(
        explored >= min_interleavings,
        "{name} explored only {explored} interleavings, below the {min_interleavings} floor"
    );
}

/// A model whose three threads all contend for one lock or semaphore.
/// The normal run explores it at preemption bound 1; an `#[ignore]`d twin
/// explores a smaller scenario of its own at bound 2, nightly
/// (`just loom-sync-deep`). The shared scenario does not finish at bound 2
/// in any useful time (43 to 79 minutes a model, and 33 hours unfinished
/// for the upgradable read), so each twin keeps one thread from parking or
/// fixes the order two waiters arrive in, and finishes in under 10
/// minutes.
macro_rules! contended {
    ($name:ident, $deep:ident, $min:literal, $model:expr, $deep_model:expr) => {
        #[test]
        fn $name() {
            explore(stringify!($name), 1, $min, $model);
        }

        #[test]
        #[ignore = "preemption bound 2 is the nightly run: `just loom-sync-deep`"]
        fn $deep() {
            explore(stringify!($deep), 2, $min, $deep_model);
        }
    };
}

struct Unpark(Park);

impl std::task::Wake for Unpark {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.notify();
    }

    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        self.0.notify();
    }
}

/// Drives `future` on this loom thread, parking it between polls until
/// its waker fires. A wakeup that never comes parks it for good, which
/// loom reports as a deadlock.
fn block_on<F: Future>(future: F) -> F::Output {
    let park = std::sync::Arc::new(Unpark(Park::new()));
    let waker = std::task::Waker::from(std::sync::Arc::clone(&park));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        park.0.wait();
    }
}

/// Raises `flag` once the wrapped future's first poll returned `Pending`,
/// so another thread can queue strictly behind it.
struct FlagWhenQueued<F> {
    inner: Pin<Box<F>>,
    flag: Arc<AtomicBool>,
    polled: bool,
}

impl<F: Future> Future for FlagWhenQueued<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let out = self.inner.as_mut().poll(cx);
        if !self.polled {
            self.polled = true;
            if out.is_pending() {
                self.flag.store(true, Ordering::Release);
            }
        }
        out
    }
}

fn flag_when_queued<F: Future>(inner: F, flag: &Arc<AtomicBool>) -> FlagWhenQueued<F> {
    FlagWhenQueued {
        inner: Box::pin(inner),
        flag: Arc::clone(flag),
        polled: false,
    }
}

fn wait_for(flag: &AtomicBool) {
    while !flag.load(Ordering::Acquire) {
        thread::yield_now();
    }
}

/// Polls `future` once with a waker that does nothing, then drops it.
fn poll_once_then_drop<F: Future>(future: F) -> bool {
    let mut future = Box::pin(future);
    let pending = future
        .as_mut()
        .poll(&mut Context::from_waker(std::task::Waker::noop()))
        .is_pending();
    drop(future);
    pending
}

fn bump(cell: &UnsafeCell<usize>) {
    cell.with_mut(|value| unsafe { *value += 1 });
}

fn read(cell: &UnsafeCell<usize>) -> usize {
    cell.with(|value| unsafe { *value })
}

#[path = "sync_loom/mutex.rs"]
mod mutex;

#[path = "sync_loom/semaphore.rs"]
mod semaphore;

#[path = "sync_loom/rwlock.rs"]
mod rwlock;

#[path = "sync_loom/reentrant.rs"]
mod reentrant;

#[path = "sync_loom/notify.rs"]
mod notify;

#[path = "sync_loom/events_and_cells.rs"]
mod events_and_cells;

#[path = "sync_loom/drain.rs"]
mod drain;

#[path = "sync_loom/transitions.rs"]
mod transitions;
