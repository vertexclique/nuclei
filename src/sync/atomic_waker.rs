//! A single-slot cell for the waker of one task, registered by that task and
//! woken by any thread.
//!
//! The cell is the classic registering/waking handshake over one state word.
//! The task that polls a future registers the waker it was polled with; the
//! thread that makes the future ready takes the registered waker and calls
//! it. A wake that comes while the task is still registering is never lost:
//! the registering task finds it on the way out and wakes itself.
//!
//! # Progress
//!
//! Every operation is wait-free. [`register`](AtomicWaker::register) runs at
//! most two compare-and-swaps and one swap, none in a loop;
//! [`take`](AtomicWaker::take) and [`wake`](AtomicWaker::wake) run two
//! read-modify-writes. No operation retries because another thread got in
//! first, and none waits for another thread: a registration that meets a
//! concurrent wake wakes the caller's own waker and returns.
//!
//! # Contract
//!
//! One task registers at a time (the oneshot channel takes `&mut self` for
//! it). A second concurrent registrant is not an error: it loses the slot,
//! its waker is woken at once, and the caller polls again.

#![allow(unsafe_code)]

use core::fmt;
use core::task::Waker;

use super::internal::{AtomicUsize, Ordering, UnsafeCell};

/// No registration and no wake is in progress; the slot may hold a waker.
const WAITING: usize = 0;
/// A task is writing the slot.
const REGISTERING: usize = 0b01;
/// A wake is taking the slot.
const WAKING: usize = 0b10;

/// A single-slot waker cell: one task registers, any thread wakes.
///
/// Public only so the loom models in `tests/` can reach it: the module is
/// crate-private in an ordinary build.
///
/// Both [`register`](Self::register) and [`take`](Self::take) are
/// wait-free (see the module docs).
pub struct AtomicWaker {
    state: AtomicUsize,
    waker: UnsafeCell<Option<Waker>>,
}

// SAFETY: the slot is touched only by the holder of REGISTERING or of
// WAKING, and the state word never lets the two be held at once.
unsafe impl Send for AtomicWaker {}
// SAFETY: as above.
unsafe impl Sync for AtomicWaker {}

impl AtomicWaker {
    loom_const_fn! {
        /// An empty cell.
        pub fn new() -> Self {
            Self { state: AtomicUsize::new(WAITING), waker: UnsafeCell::new(None) }
        }
    }

    /// Registers `waker` to be woken by the next [`wake`](Self::wake) or
    /// [`take`](Self::take), replacing the one registered before.
    ///
    /// A caller that finds a wake in progress, or another registration,
    /// wakes `waker` itself instead of waiting: it is polled again and
    /// re-checks whatever it was waiting for. The check after a
    /// registration is the caller's, as with any waker slot: a wake that
    /// came before the registration took nothing.
    pub fn register(&self, waker: &Waker) {
        // Acquire: pairs with the Release that last gave the slot up.
        if self
            .state
            .compare_exchange(WAITING, REGISTERING, Ordering::Acquire, Ordering::Acquire)
            .is_err()
        {
            waker.wake_by_ref();
            return;
        }
        // SAFETY: REGISTERING is held, so no wake touches the slot.
        self.waker.with_mut(|slot| unsafe {
            match &*slot {
                Some(old) if old.will_wake(waker) => {}
                _ => *slot = Some(waker.clone()),
            }
        });
        // Release: publishes the slot to the next wake. It fails only when a
        // wake set WAKING meanwhile, which then left the slot to us.
        if self
            .state
            .compare_exchange(REGISTERING, WAITING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // SAFETY: the state is REGISTERING | WAKING, which no other
            // thread changes, and the wake did not touch the slot.
            let taken = self.waker.with_mut(|slot| unsafe { (*slot).take() });
            // Release: hands the empty slot to whoever takes the cell next.
            self.state.swap(WAITING, Ordering::AcqRel);
            if let Some(taken) = taken {
                taken.wake();
            }
        }
    }

    /// Takes the registered waker, if a task is registered, leaving the
    /// cell empty. A take that meets a registration in progress takes
    /// nothing and leaves the registrant to wake itself.
    pub fn take(&self) -> Option<Waker> {
        // AcqRel: acquires the slot from the last registration and, when
        // WAKING was raised over a registration, tells it to wake itself.
        if self.state.fetch_or(WAKING, Ordering::AcqRel) != WAITING {
            return None;
        }
        // SAFETY: WAKING was raised over WAITING, so no registration is in
        // progress and a new one fails its first compare-and-swap.
        let taken = self.waker.with_mut(|slot| unsafe { (*slot).take() });
        // Release: hands the slot back, empty, to the next registration.
        self.state.fetch_and(!WAKING, Ordering::Release);
        taken
    }

    /// Wakes the registered waker, if any, and clears the cell.
    pub fn wake(&self) {
        if let Some(waker) = self.take() {
            waker.wake();
        }
    }
}

impl fmt::Debug for AtomicWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicWaker").finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize as StdAtomicUsize, Ordering as StdOrdering};
    use std::task::Wake;
    use std::time::{Duration, Instant};

    struct Counter(StdAtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, StdOrdering::SeqCst);
        }
    }

    fn counting() -> (Arc<Counter>, Waker) {
        let counter = Arc::new(Counter(StdAtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        (counter, waker)
    }

    fn wakes(counter: &Counter) -> usize {
        counter.0.load(StdOrdering::SeqCst)
    }

    #[test]
    fn a_registered_waker_is_woken_once_and_the_cell_empties() {
        let cell = AtomicWaker::new();
        let (counter, waker) = counting();
        cell.register(&waker);
        cell.wake();
        assert_eq!(wakes(&counter), 1);
        cell.wake();
        assert_eq!(wakes(&counter), 1, "the cell was emptied by the first wake");
    }

    #[test]
    fn a_wake_before_any_registration_takes_nothing() {
        let cell = AtomicWaker::new();
        cell.wake();
        assert!(cell.take().is_none());
        let (counter, waker) = counting();
        cell.register(&waker);
        assert_eq!(wakes(&counter), 0, "an earlier wake is not replayed");
        cell.wake();
        assert_eq!(wakes(&counter), 1);
    }

    #[test]
    fn a_new_registration_replaces_the_old_one() {
        let cell = AtomicWaker::new();
        let (first, first_waker) = counting();
        let (second, second_waker) = counting();
        cell.register(&first_waker);
        cell.register(&second_waker);
        cell.wake();
        assert_eq!((wakes(&first), wakes(&second)), (0, 1));
    }

    #[test]
    fn registering_the_same_waker_again_keeps_one_wake() {
        let cell = AtomicWaker::new();
        let (counter, waker) = counting();
        cell.register(&waker);
        cell.register(&waker.clone());
        cell.wake();
        assert_eq!(wakes(&counter), 1);
    }

    #[test]
    fn take_hands_the_waker_to_the_caller() {
        let cell = AtomicWaker::new();
        let (counter, waker) = counting();
        cell.register(&waker);
        let taken = cell.take().expect("a waker was registered");
        assert_eq!(wakes(&counter), 0, "take does not call the waker");
        taken.wake();
        assert_eq!(wakes(&counter), 1);
        assert!(cell.take().is_none());
    }

    #[test]
    fn a_registration_that_meets_a_wake_in_progress_wakes_itself() {
        let cell = AtomicWaker::new();
        cell.state.store(WAKING, StdOrdering::SeqCst);
        let (counter, waker) = counting();
        cell.register(&waker);
        assert_eq!(wakes(&counter), 1, "the contended register woke its caller");
        cell.state.store(WAITING, StdOrdering::SeqCst);
        cell.wake();
        assert_eq!(wakes(&counter), 1, "nothing was registered");
    }

    /// A waker whose first clone runs a wake against the cell, which is
    /// what a clone made by `register` does while REGISTERING is held: the
    /// wake lands in the middle of the registration.
    struct Interrupting {
        cell: *const AtomicWaker,
        clones: StdAtomicUsize,
        wakes: StdAtomicUsize,
    }

    // SAFETY: the cell pointer is used only on the test's thread, and the
    // counters are atomics; the test keeps the cell alive for every waker.
    unsafe impl Send for Interrupting {}
    // SAFETY: as above.
    unsafe impl Sync for Interrupting {}

    static INTERRUPTING: std::task::RawWakerVTable = std::task::RawWakerVTable::new(
        interrupting_clone,
        interrupting_wake,
        interrupting_wake_by_ref,
        interrupting_drop,
    );

    // Each of the four functions below is the vtable entry of a waker built
    // from `Arc::into_raw` of an `Interrupting`, which is their contract.
    unsafe fn interrupting_clone(data: *const ()) -> std::task::RawWaker {
        // SAFETY: `data` came from `Arc::into_raw` of an `Interrupting`, and
        // the cell it points to outlives every waker in the test.
        let hook = unsafe { &*(data as *const Interrupting) };
        if hook.clones.fetch_add(1, StdOrdering::SeqCst) == 0 {
            // SAFETY: as above.
            let landed = unsafe { &*hook.cell }.take();
            assert!(landed.is_none(), "a wake over a registration takes nothing");
        }
        // SAFETY: `data` is a live `Arc` pointer; this adds the clone's count.
        unsafe { Arc::increment_strong_count(data as *const Interrupting) };
        std::task::RawWaker::new(data, &INTERRUPTING)
    }

    unsafe fn interrupting_wake(data: *const ()) {
        // SAFETY: consumes the count this waker owns.
        let hook = unsafe { Arc::from_raw(data as *const Interrupting) };
        hook.wakes.fetch_add(1, StdOrdering::SeqCst);
    }

    unsafe fn interrupting_wake_by_ref(data: *const ()) {
        // SAFETY: `data` is a live `Arc` pointer.
        unsafe { &*(data as *const Interrupting) }
            .wakes
            .fetch_add(1, StdOrdering::SeqCst);
    }

    unsafe fn interrupting_drop(data: *const ()) {
        // SAFETY: consumes the count this waker owns.
        drop(unsafe { Arc::from_raw(data as *const Interrupting) });
    }

    #[test]
    fn a_wake_that_lands_during_registration_is_delivered_by_the_registrant() {
        let cell = AtomicWaker::new();
        let hook = Arc::new(Interrupting {
            cell: &cell,
            clones: StdAtomicUsize::new(0),
            wakes: StdAtomicUsize::new(0),
        });
        // SAFETY: the vtable's functions uphold the `Arc::into_raw` contract.
        let waker = unsafe {
            Waker::from_raw(std::task::RawWaker::new(
                Arc::into_raw(Arc::clone(&hook)) as *const (),
                &INTERRUPTING,
            ))
        };
        cell.register(&waker);
        assert_eq!(hook.clones.load(StdOrdering::SeqCst), 1);
        assert_eq!(
            hook.wakes.load(StdOrdering::SeqCst),
            1,
            "the registrant woke itself for the wake it could not take"
        );
        assert_eq!(cell.state.load(StdOrdering::SeqCst), WAITING);
        assert!(cell.take().is_none(), "the slot was emptied");
    }

    /// A wake racing a registration is delivered or the registrant sees what
    /// the waker set: the lost-wakeup check, many rounds on two threads.
    #[test]
    fn no_wakeup_is_lost_between_a_register_and_a_wake() {
        let rounds = if cfg!(miri) { 20 } else { 20_000 };
        let deadline = Instant::now() + Duration::from_secs(60);
        for _ in 0..rounds {
            let cell = Arc::new(AtomicWaker::new());
            let ready = Arc::new(AtomicBool::new(false));
            let (counter, waker) = counting();
            let waking = {
                let (cell, ready) = (Arc::clone(&cell), Arc::clone(&ready));
                std::thread::spawn(move || {
                    ready.store(true, StdOrdering::SeqCst);
                    cell.wake();
                })
            };
            cell.register(&waker);
            while !ready.load(StdOrdering::SeqCst) && wakes(&counter) == 0 {
                assert!(Instant::now() < deadline, "a wakeup was lost");
                std::hint::spin_loop();
            }
            waking.join().expect("waking thread");
        }
    }

    #[test]
    fn repeated_registration_from_one_thread_against_a_waking_thread_never_loses_the_last_wake() {
        let rounds = if cfg!(miri) { 20 } else { 5_000 };
        let deadline = Instant::now() + Duration::from_secs(60);
        for _ in 0..rounds {
            let cell = Arc::new(AtomicWaker::new());
            let ready = Arc::new(AtomicBool::new(false));
            let (counter, waker) = counting();
            let (other_counter, other) = counting();
            let waking = {
                let (cell, ready) = (Arc::clone(&cell), Arc::clone(&ready));
                std::thread::spawn(move || {
                    ready.store(true, StdOrdering::SeqCst);
                    cell.wake();
                })
            };
            cell.register(&other);
            cell.register(&waker);
            // Woken or ready: the wake either took one of the two or came
            // before both, in which case `ready` was set before this load.
            while !ready.load(StdOrdering::SeqCst) && wakes(&counter) + wakes(&other_counter) == 0 {
                assert!(Instant::now() < deadline, "a wakeup was lost");
                std::hint::spin_loop();
            }
            waking.join().expect("waking thread");
        }
    }
}
