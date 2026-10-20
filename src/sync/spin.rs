//! The short spin of a contended acquire, and the per-lock budget that
//! sizes it.
//!
//! A caller that finds a lock held while nobody is queued spins on the
//! state word before it queues: it reads the word, pausing between reads,
//! and tries the compare-and-swap only when a read shows room. A queued
//! waiter that loses its retry spins on its own waiter node before it
//! sleeps: a release that nudges or grants it meanwhile finds it unarmed,
//! sets the mark and calls no waker, and the spin sees the mark. Either
//! way a hold that ends within the spin costs the waiter neither a queue
//! entry nor a sleep and a wake.
//!
//! How long to spin is a per-lock count of rounds, adapted to how long
//! holds last: a spin that saw what it waited for doubles it, one that ran
//! out halves it, up to [`MAX`]. Where the holder cannot run while the
//! caller spins (two tasks of one thread) every spin runs out and the
//! count falls to zero, where nobody spins; a queued waiter whose retry
//! wins sets it to one again, so a lock whose holds turned short starts
//! spinning again. Nothing reads a clock, and the count is a hint: a lost
//! update between two threads only mis-sizes a spin.

use super::internal::{AtomicU8, Ordering};

/// The most rounds one spin makes, counted in pause hints
/// (`core::hint::spin_loop`). On the Intel Cascade Lake cores this was
/// measured on, a pause is about 140 cycles (about 37 ns), so the cap is
/// about 5 microseconds, close to what a futex-based lock spins before it
/// sleeps. The pause is shorter on many other cores (Intel cores before
/// Skylake, AMD cores, and ARM's `isb` or `yield`; not measured here). There
/// the same 128 rounds last several times less: the
/// budget still doubles and halves with what spins see, but its ceiling in
/// time is lower, so a holder that keeps the lock a few microseconds sees
/// its waiters queue sooner than on the Intel host. Calibrating the count
/// at start-up would need a clock read, which this module does not make.
// vertexia: the cap counts pauses, not time; a per-target cap measured once
// at build or start-up if spins on short-pause cores prove too short.
#[cfg(not(loom))]
pub(super) const MAX: u8 = 128;
/// Under loom one round, so the models explore a spin that runs out and
/// one that sees what it waited for, and stay small.
#[cfg(loom)]
pub(super) const MAX: u8 = 1;

/// The rounds a fresh lock starts with.
#[cfg(not(loom))]
const START: u8 = 64;
#[cfg(loom)]
const START: u8 = 1;

/// No spin where nobody else can run while the caller spins: a
/// single-threaded target.
const SPINLESS: bool = cfg!(all(target_family = "wasm", not(target_feature = "atomics")));

/// One round's pause: the processor's spin hint, or under loom a yield to
/// the model's scheduler, which a spin needs to let the thread it waits
/// for run.
#[inline]
pub(super) fn pause() {
    #[cfg(loom)]
    loom::hint::spin_loop();
    #[cfg(not(loom))]
    core::hint::spin_loop();
}

/// The longest gap between two loads of a spin on the state word, in
/// pauses.
#[cfg(not(loom))]
const GAP_MAX: u32 = 16;
#[cfg(loom)]
const GAP_MAX: u32 = 1;

/// The pauses of a spin on the state word: gaps that double from one
/// pause up to [`GAP_MAX`], until the budget is spent. Each load of the
/// word pulls its line from the holder, slowing the holder's release and
/// its next acquire; a waiter that looks seldom leaves the holder to run
/// at full speed, and still sees a release within one gap.
pub(super) struct Gaps {
    gap: u32,
    left: u32,
}

impl Gaps {
    pub(super) fn new(rounds: u32) -> Self {
        Self {
            gap: 1,
            left: rounds,
        }
    }

    /// Pauses for the next gap; false once the budget is spent.
    #[inline]
    pub(super) fn wait(&mut self) -> bool {
        if self.left == 0 {
            return false;
        }
        let gap = self.gap.min(self.left);
        for _ in 0..gap {
            pause();
        }
        self.left -= gap;
        self.gap = (self.gap * 2).min(GAP_MAX);
        true
    }
}

/// The budget of a primitive that never spins: its holders keep it across
/// long waits of their own, so a spin could only burn the caller's time.
const NEVER: u8 = u8::MAX;

/// A lock's spin budget: how many rounds its next spin makes.
pub(super) struct Budget(AtomicU8);

impl Budget {
    loom_const_fn! {
        pub(super) fn new() -> Self {
            Self(AtomicU8::new(START))
        }
    }

    loom_const_fn! {
        /// A budget that stays at zero rounds for good: no report of a won
        /// or spent spin, and no won retry, ever raises it.
        pub(super) fn never() -> Self {
            Self(AtomicU8::new(NEVER))
        }
    }

    /// The rounds a spin starting now makes; zero means no spin.
    #[inline]
    pub(super) fn rounds(&self) -> u32 {
        if SPINLESS {
            return 0;
        }
        match self.0.load(Ordering::Relaxed) {
            NEVER => 0,
            rounds => u32::from(rounds),
        }
    }

    /// A spin of `rounds` saw what it waited for: the next one may spin
    /// twice as long. A queued waiter whose retry won reports a spin of
    /// zero rounds, which brings a budget of zero back to one.
    pub(super) fn won(&self, rounds: u32) {
        self.store(rounds, (rounds.saturating_mul(2)).clamp(1, u32::from(MAX)));
    }

    /// A spin of `rounds` ran out: the next one spins half as long.
    pub(super) fn ran_out(&self, rounds: u32) {
        self.store(rounds, rounds / 2);
    }

    /// Stores `next` unless it is what the spin started from: a lock whose
    /// spins keep their length writes nothing to the line its state word
    /// lives on.
    fn store(&self, from: u32, next: u32) {
        if next != from
            && let Ok(next) = u8::try_from(next)
            && self.0.load(Ordering::Relaxed) != NEVER
        {
            self.0.store(next, Ordering::Relaxed);
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn a_won_spin_doubles_up_to_the_cap_and_a_run_out_halves_to_zero() {
        let budget = Budget::new();
        assert_eq!(budget.rounds(), u32::from(START));
        budget.won(budget.rounds());
        assert_eq!(budget.rounds(), 2 * u32::from(START));
        for _ in 0..8 {
            budget.won(budget.rounds());
        }
        assert_eq!(budget.rounds(), u32::from(MAX), "doubling stops at the cap");
        for _ in 0..8 {
            budget.ran_out(budget.rounds());
        }
        assert_eq!(budget.rounds(), 0, "halving reaches zero");
        budget.ran_out(budget.rounds());
        assert_eq!(budget.rounds(), 0);
        budget.won(0);
        assert_eq!(budget.rounds(), 1, "a won retry brings a spent budget back");
    }

    #[test]
    fn a_budget_that_never_spins_stays_at_zero() {
        let budget = Budget::never();
        assert_eq!(budget.rounds(), 0);
        budget.won(0);
        budget.won(4);
        budget.ran_out(4);
        assert_eq!(
            budget.rounds(),
            0,
            "no report raises a budget that never spins"
        );
    }

    #[test]
    fn a_spin_that_keeps_its_length_writes_nothing_and_the_last_report_wins() {
        let budget = Budget::new();
        for _ in 0..8 {
            budget.won(budget.rounds());
        }
        // A spin at the cap that won leaves the cap as it is, unwritten.
        budget.won(u32::from(MAX));
        assert_eq!(budget.rounds(), u32::from(MAX));
        // Two threads report from what each read: the last report stands,
        // even one sized from a budget that has changed since.
        budget.won(4);
        assert_eq!(budget.rounds(), 8);
        budget.ran_out(8);
        assert_eq!(budget.rounds(), 4);
    }
}
