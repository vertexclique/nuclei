//! The handshake between a shard going to sleep and a thread that gives it
//! work.
//!
//! The shard publishes that it parks, then looks for work once more; a
//! waker makes its work visible (pushes a runnable, sets a flag), then looks
//! whether the shard parks. Every one of these accesses is sequentially
//! consistent, so they fall in one total order: whichever of the two
//! publications comes second in it, the look that follows it sees the other.
//! So either the shard finds the work and does not sleep, or the waker finds
//! the shard parked and notifies it: no wake is lost. (A store and a later
//! load of different words are the one pair the weaker orderings let pass
//! each other; the measurement in `syscore/linux/bench/wake_handshake.rs`
//! lost wakes on hardware without the sequentially consistent pair.)
//!
//! The waker that swaps `parked` back to false notifies; any other waker
//! finds it false and stays quiet, so a burst of wakes costs one
//! notification. `proofs/tla/park/` checks the protocol and the loom models
//! below check this code.

use crate::sync::internal::{AtomicBool, Ordering, seq_cst_handshake};

/// A shard's half of the park handshake and its wakers' half.
pub(crate) struct ParkState {
    parked: AtomicBool,
}

impl ParkState {
    pub(crate) fn new() -> Self {
        Self {
            parked: AtomicBool::new(false),
        }
    }

    /// The shard is about to sleep: it publishes that, then looks for work
    /// with `has_work` once more. Returns true when it may sleep; false when
    /// work came first (it stays awake, `parked` is cleared again).
    pub(crate) fn begin_park(&self, has_work: impl FnOnce() -> bool) -> bool {
        self.parked.store(true, Ordering::SeqCst);
        seq_cst_handshake();
        if has_work() {
            self.parked.store(false, Ordering::SeqCst);
            false
        } else {
            true
        }
    }

    /// Whether the shard says it parks, for a test that failed.
    #[cfg(test)]
    pub(crate) fn is_parked(&self) -> bool {
        self.parked.load(Ordering::SeqCst)
    }

    /// The shard woke. A waker may have cleared `parked` already.
    pub(crate) fn end_park(&self) {
        self.parked.store(false, Ordering::SeqCst);
    }

    /// Called after the caller made its work visible with a sequentially
    /// consistent write: runs `notify` if the shard parks and no other waker
    /// notified it since.
    pub(crate) fn wake(&self, notify: impl FnOnce()) {
        seq_cst_handshake();
        if self.parked.load(Ordering::SeqCst) && self.parked.swap(false, Ordering::SeqCst) {
            notify();
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use loom::thread;

    use super::ParkState;

    /// The poller of the model: a count of notifications and whether the
    /// shard slept through one it needed.
    struct Model {
        state: ParkState,
        work: AtomicBool,
        notified: AtomicUsize,
    }

    /// A waker publishes work while the shard tries to park: the shard either
    /// sees the work, or is notified.
    #[test]
    fn a_wake_racing_the_park_is_never_lost() {
        loom::model(|| {
            let m = Arc::new(Model {
                state: ParkState::new(),
                work: AtomicBool::new(false),
                notified: AtomicUsize::new(0),
            });
            let waker = {
                let m = m.clone();
                thread::spawn(move || {
                    m.work.store(true, Ordering::SeqCst);
                    m.state.wake(|| {
                        m.notified.fetch_add(1, Ordering::SeqCst);
                    });
                })
            };
            let sleeps = m.state.begin_park(|| m.work.load(Ordering::SeqCst));
            waker.join().unwrap();
            if sleeps {
                // It slept: the wake must have notified it.
                assert_eq!(m.notified.load(Ordering::SeqCst), 1, "a wake was lost");
                m.state.end_park();
            }
            assert!(m.work.load(Ordering::SeqCst));
        });
    }

    /// The handshake with one rule taken away, for the calibrations: without
    /// the shard's look after it publishes, or with the pair's accesses
    /// relaxed and no fence, a wake must be lost in some interleaving.
    fn broken_park(recheck: bool, ordering: Ordering) {
        let parked = Arc::new(AtomicBool::new(false));
        let work = Arc::new(AtomicBool::new(false));
        let notified = Arc::new(AtomicUsize::new(0));
        let waker = {
            let (parked, work, notified) = (parked.clone(), work.clone(), notified.clone());
            thread::spawn(move || {
                work.store(true, ordering);
                if ordering == Ordering::SeqCst {
                    loom::sync::atomic::fence(Ordering::SeqCst);
                }
                if parked.load(ordering) {
                    notified.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        parked.store(true, ordering);
        if ordering == Ordering::SeqCst {
            loom::sync::atomic::fence(Ordering::SeqCst);
        }
        let sleeps = !(recheck && work.load(ordering));
        waker.join().unwrap();
        if sleeps {
            assert_eq!(notified.load(Ordering::SeqCst), 1, "a wake was lost");
        }
    }

    #[test]
    #[should_panic(expected = "a wake was lost")]
    fn calibration_a_park_without_its_look_loses_a_wake() {
        loom::model(|| broken_park(false, Ordering::SeqCst));
    }

    #[test]
    #[should_panic(expected = "a wake was lost")]
    fn calibration_a_relaxed_pair_without_a_fence_loses_a_wake() {
        loom::model(|| broken_park(true, Ordering::Relaxed));
    }

    #[test]
    fn calibration_the_sequentially_consistent_pair_keeps_every_wake() {
        loom::model(|| broken_park(true, Ordering::SeqCst));
    }

    /// Two wakers: at most one notification, and at least one if the shard
    /// sleeps.
    #[test]
    fn a_burst_of_wakes_notifies_once() {
        loom::model(|| {
            let m = Arc::new(Model {
                state: ParkState::new(),
                work: AtomicBool::new(false),
                notified: AtomicUsize::new(0),
            });
            let wakers: Vec<_> = (0..2)
                .map(|_| {
                    let m = m.clone();
                    thread::spawn(move || {
                        m.work.store(true, Ordering::SeqCst);
                        m.state.wake(|| {
                            m.notified.fetch_add(1, Ordering::SeqCst);
                        });
                    })
                })
                .collect();
            let sleeps = m.state.begin_park(|| m.work.load(Ordering::SeqCst));
            for w in wakers {
                w.join().unwrap();
            }
            let notified = m.notified.load(Ordering::SeqCst);
            assert!(notified <= 1, "{notified} notifications for one park");
            if sleeps {
                assert_eq!(notified, 1, "a wake was lost");
            }
        });
    }
}
