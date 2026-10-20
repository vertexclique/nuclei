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
    ReentrantRwLock, RwLock, Semaphore,
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

contended!(
    mutex_excludes_and_loses_no_wakeup,
    mutex_excludes_and_loses_no_wakeup_at_bound_2,
    10,
    // SHRUNK: the main thread holds the lock from the start, where all three
    // threads used to race for it (462 thousand interleavings, 58 s). Two
    // threads still wait for one lock and each must be woken: the release
    // hands it to one, whose release hands it to the other, and a barging
    // waiter may take it between the two. What is left out is a third
    // thread parking: the holder never waits.
    || {
        let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
        let held = mutex.try_lock().expect("free");
        let others: Vec<_> = (0..2)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                thread::spawn(move || bump(&block_on(mutex.lock())))
            })
            .collect();
        bump(&held);
        drop(held);
        for other in others {
            other.join().expect("locker");
        }
        assert_eq!(read(&mutex.try_lock().expect("free at the end")), 3);
    },
    // The bound-2 twin: one thread waits and one only tries (`try_lock`), so
    // a single thread parks. What races at bound 2 is the release against the
    // waiter's arrival and against a caller barging in ahead of it.
    || {
        let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
        let held = mutex.try_lock().expect("free");
        let waiter = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || bump(&block_on(mutex.lock())))
        };
        let barger = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || mutex.try_lock().map(|guard| bump(&guard)).is_some())
        };
        bump(&held);
        drop(held);
        waiter.join().expect("waiter");
        let barged = barger.join().expect("barger");
        assert_eq!(
            read(&mutex.try_lock().expect("free at the end")),
            2 + usize::from(barged)
        );
    }
);

#[test]
fn a_waiter_barged_past_max_bypass_times_is_handed_the_mutex() {
    explore(
        "a_waiter_barged_past_max_bypass_times_is_handed_the_mutex",
        2,
        10,
        || {
            let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
            let mut held = Some(mutex.try_lock().expect("free"));
            let queued = Arc::new(AtomicBool::new(false));
            let waiter = {
                let (mutex, flag) = (Arc::clone(&mutex), Arc::clone(&queued));
                thread::spawn(move || bump(&block_on(flag_when_queued(mutex.lock(), &flag))))
            };
            wait_for(&queued);
            // Barge back in after every release, once more than the bound
            // allows: past it the release must go to the waiter.
            let mut barged = 0;
            for _ in 0..=MAX_BYPASS {
                drop(held.take());
                held = mutex.try_lock();
                if let Some(guard) = &held {
                    bump(guard);
                    barged += 1;
                }
            }
            drop(held);
            waiter.join().expect("waiter");
            assert_eq!(read(&mutex.try_lock().expect("free")), barged + 1);
        },
    );
}

contended!(
    a_cancelled_mutex_waiter_racing_the_handoff_loses_nothing,
    a_cancelled_mutex_waiter_racing_the_handoff_loses_nothing_at_bound_2,
    10,
    || {
        let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
        let held = mutex.try_lock().expect("free");
        let canceller = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || poll_once_then_drop(mutex.lock()))
        };
        let waiter = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || bump(&block_on(mutex.lock())))
        };
        drop(held);
        canceller.join().expect("canceller");
        waiter.join().expect("waiter");
        assert_eq!(read(&mutex.try_lock().expect("the lock was not leaked")), 1);
    },
    // The bound-2 twin: the canceller is queued before the waiter exists, so
    // the order of the two arrivals is fixed and only the cancellation races
    // the waiter's arrival and the release. That is the case the model is
    // for: a handoff given to a waiter that is going away must reach the one
    // behind it.
    || {
        let mutex = Arc::new(Mutex::new(UnsafeCell::new(0usize)));
        let held = mutex.try_lock().expect("free");
        let queued = Arc::new(AtomicBool::new(false));
        let canceller = {
            let (mutex, flag) = (Arc::clone(&mutex), Arc::clone(&queued));
            thread::spawn(move || poll_once_then_drop(flag_when_queued(mutex.lock(), &flag)))
        };
        wait_for(&queued);
        let waiter = {
            let mutex = Arc::clone(&mutex);
            thread::spawn(move || bump(&block_on(mutex.lock())))
        };
        drop(held);
        canceller.join().expect("canceller");
        waiter.join().expect("waiter");
        assert_eq!(read(&mutex.try_lock().expect("the lock was not leaked")), 1);
    }
);

contended!(
    a_semaphore_never_hands_out_more_permits_than_it_has,
    a_semaphore_never_hands_out_more_permits_than_it_has_at_bound_2,
    10,
    || {
        let sem = Arc::new(Semaphore::new(2));
        let out = Arc::new(AtomicUsize::new(0));
        let held = sem.try_acquire(2).expect("free");
        let takers: Vec<_> = [2, 1]
            .into_iter()
            .map(|n| {
                let (sem, out) = (Arc::clone(&sem), Arc::clone(&out));
                thread::spawn(move || {
                    let permit = block_on(sem.acquire(n));
                    assert!(out.fetch_add(n, Ordering::SeqCst) + n <= 2, "overdrawn");
                    out.fetch_sub(n, Ordering::SeqCst);
                    drop(permit);
                })
            })
            .collect();
        drop(held);
        for taker in takers {
            taker.join().expect("taker");
        }
        assert_eq!(sem.available_permits(), 2);
    },
    // The bound-2 twin: the request for one permit only tries
    // (`try_acquire`), so a single thread parks. What races at bound 2 is
    // the release against the large request's arrival and against the small
    // caller taking a permit ahead of it.
    || {
        let sem = Arc::new(Semaphore::new(2));
        let out = Arc::new(AtomicUsize::new(0));
        let held = sem.try_acquire(2).expect("free");
        let big = {
            let (sem, out) = (Arc::clone(&sem), Arc::clone(&out));
            thread::spawn(move || {
                let permit = block_on(sem.acquire(2));
                assert!(out.fetch_add(2, Ordering::SeqCst) + 2 <= 2, "overdrawn");
                out.fetch_sub(2, Ordering::SeqCst);
                drop(permit);
            })
        };
        let small = {
            let (sem, out) = (Arc::clone(&sem), Arc::clone(&out));
            thread::spawn(move || {
                if let Some(permit) = sem.try_acquire(1) {
                    assert!(out.fetch_add(1, Ordering::SeqCst) + 1 <= 2, "overdrawn");
                    out.fetch_sub(1, Ordering::SeqCst);
                    drop(permit);
                }
            })
        };
        drop(held);
        big.join().expect("big");
        small.join().expect("small");
        assert_eq!(sem.available_permits(), 2);
    }
);

#[test]
fn a_large_request_behind_small_callers_is_owed_its_permits() {
    explore(
        "a_large_request_behind_small_callers_is_owed_its_permits",
        // SHRUNK: two preemptions where there were three (4.7 million
        // interleavings, over 5 minutes; now about 200 thousand). The
        // scenario is unchanged: the large request still meets a small
        // caller barging past it more often than the bound allows, and two
        // preemptions still split the release, the barge and the handoff.
        2,
        5,
        || {
            let sem = Arc::new(Semaphore::new(2));
            let mut small = sem.try_acquire(1);
            let other = sem.try_acquire(1).expect("free");
            let queued = Arc::new(AtomicBool::new(false));
            let big = {
                let (sem, flag) = (Arc::clone(&sem), Arc::clone(&queued));
                thread::spawn(move || block_on(flag_when_queued(sem.acquire(2), &flag)).count())
            };
            wait_for(&queued);
            // One permit at a time comes free and a small caller takes it
            // whenever it may, once more than the bound allows.
            for _ in 0..=MAX_BYPASS {
                drop(small.take());
                small = sem.try_acquire(1);
            }
            drop(small);
            drop(other);
            assert_eq!(big.join().expect("big"), 2);
            assert_eq!(sem.available_permits(), 2);
        },
    );
}

contended!(
    an_upgradable_read_upgrades_while_readers_and_a_writer_contend,
    an_upgradable_read_upgrades_while_readers_and_a_writer_contend_at_bound_2,
    10,
    || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let upgrader = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let upgradable = block_on(lock.upgradable_read());
                let _ = read(&upgradable);
                bump(&block_on(upgradable.upgrade()));
            })
        };
        let reader = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let _ = read(&block_on(lock.read()));
            })
        };
        bump(&block_on(lock.write()));
        upgrader.join().expect("upgrader");
        reader.join().expect("reader");
        assert_eq!(read(&lock.try_write().expect("free")), 2);
    },
    // The bound-2 twin runs a smaller scenario. The one above did not finish
    // at bound 2 (a run was stopped after 33 hours): three threads each park
    // and wake on the lock, and two preemptions among them is too many
    // interleavings. Here the upgradable read is held from the start, so no
    // thread waits for it, and the reader only tries (`try_read`), so it
    // never parks. What still races at bound 2 is the upgrade against a
    // reader coming or going and against a writer queued behind it, with
    // every wake the upgrade and the writer owe each other. Measured: 171149
    // interleavings in 10 s. With a blocking reader in place of the trying
    // one it did not finish in 30 minutes.
    || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let upgradable = lock.try_upgradable_read().expect("free");
        let reader = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let _ = lock.try_read().map(|shared| read(&shared));
            })
        };
        let writer = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || bump(&block_on(lock.write())))
        };
        bump(&block_on(upgradable.upgrade()));
        reader.join().expect("reader");
        writer.join().expect("writer");
        assert_eq!(read(&lock.try_write().expect("free")), 2);
    }
);

#[test]
fn a_dropped_upgrade_racing_the_last_reader_leaves_nothing_held() {
    explore(
        "a_dropped_upgrade_racing_the_last_reader_leaves_nothing_held",
        3,
        5,
        || {
            let lock = Arc::new(RwLock::new(()));
            let upgradable = lock.try_upgradable_read().expect("free");
            let reader = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || drop(lock.try_read()))
            };
            poll_once_then_drop(upgradable.upgrade());
            reader.join().expect("reader");
            assert!(lock.try_write().is_some(), "nothing was left held");
        },
    );
}

#[test]
fn a_rwlock_writer_and_readers_exclude_each_other() {
    explore(
        "a_rwlock_writer_and_readers_exclude_each_other",
        // SHRUNK: one preemption where there were two (2.5 million
        // interleavings, 4 minutes; now about 96 thousand), as the other
        // models whose three threads all wait for one lock run. The scenario
        // is unchanged: a writer and two readers, any of them first.
        1,
        10,
        || {
            let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
            let writer = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || bump(&block_on(lock.write())))
            };
            let reader = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    let _ = read(&block_on(lock.read()));
                })
            };
            let _ = read(&block_on(lock.read()));
            writer.join().expect("writer");
            reader.join().expect("reader");
            assert_eq!(read(&lock.try_write().expect("free")), 1);
        },
    );
}

#[test]
fn a_reentrant_mutex_unwinds_its_depth_before_another_owner_enters() {
    explore(
        "a_reentrant_mutex_unwinds_its_depth_before_another_owner_enters",
        3,
        5,
        || {
            let lock = Arc::new(ReentrantMutex::new(UnsafeCell::new(0usize)));
            let other = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    let owner = Owner::new();
                    bump(&block_on(lock.lock(&owner)));
                })
            };
            let owner = Owner::new();
            let outer = block_on(lock.lock(&owner));
            let inner = block_on(lock.lock(&owner));
            bump(&inner);
            drop(inner);
            bump(&outer);
            drop(outer);
            other.join().expect("other owner");
            assert_eq!(read(&lock.try_lock(&owner).expect("free")), 3);
        },
    );
}

#[test]
fn a_reentrant_reader_never_waits_behind_a_writer_waiting_for_it() {
    explore(
        "a_reentrant_reader_never_waits_behind_a_writer_waiting_for_it",
        3,
        5,
        || {
            let lock = Arc::new(ReentrantRwLock::new(AtomicUsize::new(0)));
            let owner = Owner::new();
            let first = lock.try_read(&owner).expect("free");
            let writer = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    let owner = Owner::new();
                    block_on(lock.write(&owner))
                        .expect("this owner holds no read")
                        .fetch_add(1, Ordering::Relaxed);
                })
            };
            let second = block_on(lock.read(&owner));
            drop(first);
            drop(second);
            writer.join().expect("writer");
            assert_eq!(
                lock.try_read(&owner).expect("free").load(Ordering::Relaxed),
                1
            );
        },
    );
}

#[test]
fn notify_one_is_never_lost() {
    explore("notify_one_is_never_lost", 3, 5, || {
        let notify = Arc::new(Notify::new());
        let waiter = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || block_on(notify.notified()))
        };
        notify.notify_one();
        waiter.join().expect("waiter");
    });
}

#[test]
fn notify_waiters_reaches_a_registered_waiter() {
    explore("notify_waiters_reaches_a_registered_waiter", 3, 3, || {
        let notify = Arc::new(Notify::new());
        let queued = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (notify, flag) = (Arc::clone(&notify), Arc::clone(&queued));
            thread::spawn(move || block_on(flag_when_queued(notify.notified(), &flag)))
        };
        wait_for(&queued);
        notify.notify_waiters();
        waiter.join().expect("waiter");
    });
}

/// A `Notified` created before a `notify_waiters` passes its generation
/// check and is held up before it queues; the broadcast runs, and a newer
/// waiter queues ahead of it. The older one is still owed the broadcast
/// wherever it lands in the queue: a pass that only looks at the front
/// stops at the newer waiter and leaves the older one parked for good,
/// which loom reports as a deadlock.
#[test]
fn an_older_notified_queued_behind_a_newer_one_gets_the_broadcast() {
    explore(
        "an_older_notified_queued_behind_a_newer_one_gets_the_broadcast",
        3,
        5,
        || {
            let notify = Arc::new(Notify::new());
            let created = Arc::new(AtomicBool::new(false));
            let older = {
                let (notify, created) = (Arc::clone(&notify), Arc::clone(&created));
                thread::spawn(move || {
                    let waiting = notify.notified();
                    created.store(true, Ordering::Release);
                    block_on(waiting);
                })
            };
            wait_for(&created);
            notify.notify_waiters();
            // Created after the broadcast, so it is not this one's.
            let mut newer = Box::pin(notify.notified());
            let polled = newer
                .as_mut()
                .poll(&mut Context::from_waker(std::task::Waker::noop()));
            assert!(polled.is_pending(), "nothing notified it");
            older.join().expect("older");
            drop(newer);
        },
    );
}

#[test]
fn a_dropped_notified_passes_its_notify_one_on() {
    // SHRUNK: one preemption where there were two (1.2 million interleavings,
    // 105 s; now about 62 thousand), as the other three-thread models run.
    // The scenario is unchanged: the notification still reaches a waiter
    // whether the cancelled future took it, dropped it, or never saw it.
    explore("a_dropped_notified_passes_its_notify_one_on", 1, 5, || {
        let notify = Arc::new(Notify::new());
        // Returns true when its one poll completed, which consumes the
        // notification legitimately rather than dropping it.
        let canceller = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || !poll_once_then_drop(notify.notified()))
        };
        let waiter = {
            let notify = Arc::clone(&notify);
            thread::spawn(move || block_on(notify.notified()))
        };
        notify.notify_one();
        if canceller.join().expect("canceller") {
            notify.notify_one();
        }
        waiter.join().expect("waiter");
    });
}

/// A queued `Notified` is dropped on its own thread while a broadcast's
/// wake list takes its waker. The grant let go of the queue's reference,
/// so once the wake list lets go of its own the dropped future frees the
/// node; the wake list must be done with the node by then. Under loom a
/// freed node counts as written (`Waiter::retire`), so a wake list that
/// reads the node's link after letting it go fails this model.
#[test]
fn a_notified_dropped_while_its_broadcast_wakes_it_outlives_the_wake_list() {
    explore(
        "a_notified_dropped_while_its_broadcast_wakes_it_outlives_the_wake_list",
        3,
        5,
        || {
            let notify = Arc::new(Notify::new());
            let queued = Arc::new(AtomicBool::new(false));
            let dropper = {
                let (notify, queued) = (Arc::clone(&notify), Arc::clone(&queued));
                thread::spawn(move || {
                    let mut waiting = Box::pin(notify.notified());
                    let polled = waiting
                        .as_mut()
                        .poll(&mut Context::from_waker(std::task::Waker::noop()));
                    assert!(polled.is_pending(), "nothing notified it yet");
                    queued.store(true, Ordering::Release);
                    drop(waiting);
                })
            };
            wait_for(&queued);
            notify.notify_waiters();
            dropper.join().expect("dropper");
        },
    );
}

#[test]
fn an_event_set_concurrently_with_waits_wakes_them() {
    explore(
        "an_event_set_concurrently_with_waits_wakes_them",
        2,
        5,
        || {
            let event = Arc::new(Event::new());
            let waiters: Vec<_> = (0..2)
                .map(|_| {
                    let event = Arc::clone(&event);
                    thread::spawn(move || block_on(event.wait()))
                })
                .collect();
            event.set();
            for waiter in waiters {
                waiter.join().expect("waiter");
            }
        },
    );
}

#[test]
fn a_latch_opens_after_concurrent_count_downs() {
    explore("a_latch_opens_after_concurrent_count_downs", 2, 5, || {
        let latch = Arc::new(Latch::new(2));
        let counters: Vec<_> = (0..2)
            .map(|_| {
                let latch = Arc::clone(&latch);
                thread::spawn(move || latch.count_down())
            })
            .collect();
        block_on(latch.wait());
        for counter in counters {
            counter.join().expect("counter");
        }
        assert_eq!(latch.count(), 0);
    });
}

#[test]
fn a_barrier_releases_both_with_one_leader() {
    explore("a_barrier_releases_both_with_one_leader", 3, 5, || {
        let barrier = Arc::new(Barrier::new(2));
        let other = {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || block_on(barrier.wait()).is_leader())
        };
        let mine = block_on(barrier.wait()).is_leader();
        let theirs = other.join().expect("other");
        assert!(mine ^ theirs, "exactly one leader");
    });
}

#[test]
fn once_cell_initializers_agree_on_one_value() {
    explore("once_cell_initializers_agree_on_one_value", 3, 5, || {
        let cell = Arc::new(OnceCell::new());
        let racer = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || *cell.get_or_init_racy(|| 1))
        };
        let mine = *block_on(cell.get_or_init(async { 2 }));
        assert_eq!(racer.join().expect("racer"), mine);
    });
}

contended!(
    a_cancelled_rwlock_writer_racing_the_last_reader_loses_nothing,
    a_cancelled_rwlock_writer_racing_the_last_reader_loses_nothing_at_bound_2,
    10,
    || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let held = lock.try_read().expect("free");
        let canceller = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || poll_once_then_drop(lock.write()))
        };
        let writer = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || bump(&block_on(lock.write())))
        };
        drop(held);
        canceller.join().expect("canceller");
        writer.join().expect("writer");
        assert_eq!(read(&lock.try_write().expect("the lock was not leaked")), 1);
    },
    // The bound-2 twin: the cancelled writer is queued before the other
    // writer exists, so the order of the two arrivals is fixed and only the
    // cancellation races the second writer's arrival and the last reader's
    // release.
    || {
        let lock = Arc::new(RwLock::new(UnsafeCell::new(0usize)));
        let held = lock.try_read().expect("free");
        let queued = Arc::new(AtomicBool::new(false));
        let canceller = {
            let (lock, flag) = (Arc::clone(&lock), Arc::clone(&queued));
            thread::spawn(move || poll_once_then_drop(flag_when_queued(lock.write(), &flag)))
        };
        wait_for(&queued);
        let writer = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || bump(&block_on(lock.write())))
        };
        drop(held);
        canceller.join().expect("canceller");
        writer.join().expect("writer");
        assert_eq!(read(&lock.try_write().expect("the lock was not leaked")), 1);
    }
);

#[test]
fn a_reentrant_write_becomes_its_owners_read_while_another_owner_waits() {
    explore(
        "a_reentrant_write_becomes_its_owners_read_while_another_owner_waits",
        3,
        5,
        || {
            let lock = Arc::new(ReentrantRwLock::new(AtomicUsize::new(0)));
            let owner = Owner::new();
            let write = lock.try_write(&owner).expect("free");
            let read = lock.try_read(&owner).expect("a writer may read");
            let other = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    let owner = Owner::new();
                    block_on(lock.write(&owner))
                        .expect("this owner holds no read")
                        .fetch_add(1, Ordering::Relaxed);
                })
            };
            write.fetch_add(1, Ordering::Relaxed);
            drop(write);
            assert_eq!(
                read.load(Ordering::Relaxed),
                1,
                "the read outlives the write"
            );
            drop(read);
            other.join().expect("other owner");
            assert_eq!(
                lock.try_write(&Owner::new())
                    .expect("free")
                    .load(Ordering::Relaxed),
                2
            );
        },
    );
}

#[test]
fn a_cancelled_once_cell_initializer_hands_the_turn_on() {
    explore(
        "a_cancelled_once_cell_initializer_hands_the_turn_on",
        3,
        5,
        || {
            let cell = Arc::new(OnceCell::new());
            let quitter = {
                let cell = Arc::clone(&cell);
                thread::spawn(move || poll_once_then_drop(cell.get_or_init(std::future::pending())))
            };
            assert_eq!(*block_on(cell.get_or_init(async { 2 })), 2);
            quitter.join().expect("quitter");
        },
    );
}

#[test]
fn a_set_wakes_a_waiter_queued_behind_a_running_initializer() {
    explore(
        "a_set_wakes_a_waiter_queued_behind_a_running_initializer",
        3,
        5,
        || {
            let cell = Arc::new(OnceCell::new());
            let mut stuck = Box::pin(cell.get_or_init(std::future::pending()));
            assert!(
                stuck
                    .as_mut()
                    .poll(&mut Context::from_waker(std::task::Waker::noop()))
                    .is_pending()
            );
            let waiter = {
                let cell = Arc::clone(&cell);
                thread::spawn(move || *block_on(cell.get_or_init(async { 2 })))
            };
            assert_eq!(cell.set(1), Ok(()));
            assert_eq!(waiter.join().expect("waiter"), 1);
            drop(stuck);
        },
    );
}

#[test]
fn a_dropped_barrier_wait_stays_counted_while_its_generation_completes() {
    explore(
        "a_dropped_barrier_wait_stays_counted_while_its_generation_completes",
        3,
        5,
        || {
            let barrier = Arc::new(Barrier::new(2));
            let mut first = Box::pin(barrier.wait());
            assert!(
                first
                    .as_mut()
                    .poll(&mut Context::from_waker(std::task::Waker::noop()))
                    .is_pending()
            );
            let second = {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || block_on(barrier.wait()).is_leader())
            };
            drop(first);
            assert!(
                second.join().expect("second"),
                "the dropped arrival still counts, so the second completes the pair"
            );
            // The next generation starts clean.
            let third = {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || block_on(barrier.wait()).is_leader())
            };
            let fourth = block_on(barrier.wait()).is_leader();
            assert!(third.join().expect("third") ^ fourth, "exactly one leader");
        },
    );
}

#[test]
fn a_cancelled_event_wait_racing_the_set_strands_nobody() {
    explore(
        "a_cancelled_event_wait_racing_the_set_strands_nobody",
        2,
        5,
        || {
            let event = Arc::new(Event::new());
            let canceller = {
                let event = Arc::clone(&event);
                thread::spawn(move || poll_once_then_drop(event.wait()))
            };
            let waiter = {
                let event = Arc::clone(&event);
                thread::spawn(move || block_on(event.wait()))
            };
            event.set();
            canceller.join().expect("canceller");
            waiter.join().expect("waiter");
        },
    );
}

#[test]
fn a_waiter_serving_the_queue_is_served_while_the_holder_barges() {
    nuclei::sync::loom_model::a_waiter_serving_the_queue_is_served_while_the_holder_barges();
}

#[test]
#[should_panic(expected = "the drain ran")]
fn calibration_a_marker_lowered_on_a_pending_nudge_keeps_the_drain_passing() {
    nuclei::sync::loom_model::calibration_a_marker_lowered_on_a_pending_nudge_keeps_the_drain_passing();
}

/// The Mutex's inline first attempt never takes a lock owed to a queued
/// waiter, with the spins; its calibration drops the attempt's HANDOFF test
/// and must fail.
#[test]
fn the_first_attempt_refuses_a_lock_owed_to_a_waiter() {
    nuclei::sync::loom_model::the_first_attempt_refuses_a_lock_owed_to_a_waiter();
}

#[test]
#[should_panic(expected = "the holder took a lock owed to the queued waiter")]
fn calibration_a_first_attempt_that_ignores_handoff_barges_past_an_owed_waiter() {
    nuclei::sync::loom_model::calibration_a_first_attempt_that_ignores_handoff_barges_past_an_owed_waiter();
}

/// A release that nudges a waiter spinning on its node calls no waker, and
/// the waiter is served all the same (no lost wakeup), with floors on the
/// spins it must reach; the calibration is the production spin with its look
/// taken away and must leave the waiter armed beside an unread mark.
#[test]
fn a_nudge_while_the_waiter_spins_is_never_lost() {
    nuclei::sync::loom_model::a_nudge_while_the_waiter_spins_is_never_lost();
}

#[test]
#[should_panic(expected = "the waiter armed and slept beside an unread mark")]
fn calibration_a_node_spin_that_skips_its_look_sleeps_beside_the_mark() {
    nuclei::sync::loom_model::calibration_a_node_spin_that_skips_its_look_sleeps_beside_the_mark();
}

/// An upgrade waits for a reader and is dropped. While its drop is
/// under way the reader leaves, which runs a drain pass, and a second task
/// takes the upgradable read; then the first task tries to write. The
/// upgradable read is its holder's own until that holder lets it go:
/// neither the dropped upgrade nor the pass may spend the bits the second
/// task raised, so no writer is ever inside while the second task holds it.
#[test]
fn a_dropped_upgrade_never_lets_a_writer_in_beside_the_next_upgradable_read() {
    explore(
        "a_dropped_upgrade_never_lets_a_writer_in_beside_the_next_upgradable_read",
        3,
        100,
        || {
            let lock = Arc::new(RwLock::new(()));
            let upgradable = lock.try_upgradable_read().expect("free");
            let inside = Arc::new(AtomicBool::new(false));
            let held = Arc::new(AtomicBool::new(false));
            let second = {
                let (lock, inside, held) =
                    (Arc::clone(&lock), Arc::clone(&inside), Arc::clone(&held));
                thread::spawn(move || {
                    let shared = lock.try_read().expect("readers share with it");
                    inside.store(true, Ordering::Release);
                    // Lets the upgrade begin, and its drop, while the reader
                    // is inside, without spending a preemption.
                    thread::yield_now();
                    // The reader leaves: the upgrade may be waiting for it.
                    drop(shared);
                    if let Some(next) = lock.try_upgradable_read() {
                        held.store(true, Ordering::Release);
                        // The same, while this task holds it.
                        thread::yield_now();
                        held.store(false, Ordering::Release);
                        drop(next);
                    }
                })
            };
            wait_for(&inside);
            poll_once_then_drop(upgradable.upgrade());
            if let Some(alone) = lock.try_write() {
                assert!(
                    !held.load(Ordering::Acquire),
                    "a writer got in while the upgradable read was held"
                );
                drop(alone);
            }
            second.join().expect("second");
            assert!(lock.try_write().is_some(), "nothing was left held");
        },
    );
}

/// A step the drain no longer takes, rebuilt out of plain loom atomics: a
/// drain pass that reads the upgrading bit to decide whether the upgrade in
/// its slot was dropped, and takes the write for it when no reader is inside.
/// The bit it reads may be the next holder's, and the write it takes and puts
/// back spends the upgradable read whoever holds it, so a writer gets in
/// beside that holder.
#[test]
#[should_panic(expected = "a writer got in while the upgradable read was held")]
fn calibration_a_pass_that_takes_the_write_for_a_dropped_upgrade_lets_a_writer_in() {
    const WRITER: usize = 1;
    const UPGRADABLE: usize = 1 << 1;
    const UPGRADING: usize = 1 << 2;
    const ONE_READER: usize = 1 << 3;
    explore(
        "calibration_a_pass_that_takes_the_write_for_a_dropped_upgrade_lets_a_writer_in",
        3,
        1,
        || {
            // The first task holds the upgradable read, its upgrade waits
            // for one reader, and its node is in the pass's slot.
            let state = Arc::new(AtomicUsize::new(UPGRADABLE | UPGRADING | ONE_READER));
            let withdrawn = Arc::new(AtomicBool::new(false));
            let held = Arc::new(AtomicBool::new(false));
            let second = {
                let (state, withdrawn, held) = (
                    Arc::clone(&state),
                    Arc::clone(&withdrawn),
                    Arc::clone(&held),
                );
                thread::spawn(move || {
                    // The reader leaves and runs the pass, the old way.
                    state.fetch_sub(ONE_READER, Ordering::AcqRel);
                    let seen = state.load(Ordering::Acquire);
                    if seen & UPGRADING != 0
                        && seen >> 3 == 0
                        && state
                            .compare_exchange(
                                seen,
                                (seen & !(UPGRADABLE | UPGRADING)) | WRITER,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                        && withdrawn.load(Ordering::Acquire)
                    {
                        // The node withdrew: the write taken for it goes back.
                        state.fetch_and(!WRITER, Ordering::AcqRel);
                    }
                    // Then it takes the upgradable read if it is free.
                    let free = state.load(Ordering::Acquire);
                    if free & (WRITER | UPGRADABLE) == 0
                        && state
                            .compare_exchange(
                                free,
                                free | UPGRADABLE,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                    {
                        held.store(true, Ordering::Release);
                        thread::yield_now();
                        held.store(false, Ordering::Release);
                        state.fetch_and(!UPGRADABLE, Ordering::AcqRel);
                    }
                })
            };
            // The first task drops its upgrade: its node withdraws, then it
            // lets the upgradable read go.
            withdrawn.store(true, Ordering::Release);
            state.fetch_and(!(UPGRADABLE | UPGRADING), Ordering::AcqRel);
            // Then it tries to write.
            if state
                .compare_exchange(0, WRITER, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                assert!(
                    !held.load(Ordering::Acquire),
                    "a writer got in while the upgradable read was held"
                );
                state.fetch_and(!WRITER, Ordering::AcqRel);
            }
            second.join().expect("second");
        },
    );
}

#[test]
fn a_drain_that_found_nobody_queued_ends_while_another_thread_releases() {
    nuclei::sync::loom_model::a_drain_that_found_nobody_queued_ends_while_another_thread_releases();
}

#[test]
#[should_panic(expected = "over a queue with nobody in it")]
fn calibration_a_release_that_dirties_keeps_an_empty_drain_passing() {
    nuclei::sync::loom_model::calibration_a_release_that_dirties_keeps_an_empty_drain_passing();
}

#[test]
#[should_panic(expected = "Causality violation")]
fn calibration_an_unguarded_write_is_caught() {
    explore("calibration_an_unguarded_write_is_caught", 3, 1, || {
        let cell = Arc::new(UnsafeCell::new(0usize));
        let other = {
            let cell = Arc::clone(&cell);
            thread::spawn(move || bump(&cell))
        };
        bump(&cell);
        other.join().expect("other");
    });
}

/// A queued waiter does not keep the lock from a caller that finds it
/// free: barging is the design, so expecting the queue to win must fail.
#[test]
#[should_panic(expected = "a release went to a caller ahead of the queued waiter")]
fn calibration_a_queued_waiter_can_be_barged_past() {
    explore(
        "calibration_a_queued_waiter_can_be_barged_past",
        3,
        1,
        || {
            let mutex = Arc::new(Mutex::new(()));
            let held = mutex.try_lock().expect("free");
            let queued = Arc::new(AtomicBool::new(false));
            let waiter = {
                let (mutex, flag) = (Arc::clone(&mutex), Arc::clone(&queued));
                thread::spawn(move || drop(block_on(flag_when_queued(mutex.lock(), &flag))))
            };
            wait_for(&queued);
            drop(held);
            let barged = mutex.try_lock().is_some();
            waiter.join().expect("waiter");
            assert!(
                !barged,
                "a release went to a caller ahead of the queued waiter"
            );
        },
    );
}
