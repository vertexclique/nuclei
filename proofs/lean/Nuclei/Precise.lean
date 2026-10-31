/-!
# The precise sleep's rounding

This file checks the arithmetic a precise sleep rests on (`src/rt/shard/timers.rs`:
`time_of`, `units_after`, `precise_now`; `src/time/precise.rs`): a deadline is armed in the
microsecond wheel rounded up, the wheel's time now is the clock rounded down, and a timer fires
once that time reaches its deadline's. So a precise sleep never ends before its deadline, and
the wheel never adds a microsecond to it.

## The story in plain words

The clock counts nanoseconds from one origin. A precise deadline at `d` nanoseconds is armed at
`ceil(d / 1000)` microseconds; the shard reads the clock at `now` nanoseconds as
`floor(now / 1000)` microseconds, and fires the timer once the second has reached the first.
Rounding the deadline up and the clock down is what keeps a timer from firing early: when the
clock's microsecond reaches the deadline's, the clock is past the deadline itself. A deadline
on the microsecond grid fires the moment the clock reaches it, and one off the grid at most one
microsecond after. The millisecond wheel of `sleep` follows the same two rules with a unit of a
million nanoseconds, and its timer may fire up to a millisecond after its deadline: what a
precise sleep is for.

A worked example: a deadline at 2,000,001 ns is armed at 2,001 us (and on the millisecond wheel
at 3 ms); a clock at 2,000,999 ns reads 2,000 us, not yet due; at 2,001,000 ns it reads 2,001
us, due, and the clock is past the deadline.

## What the model leaves out

The wheel's placement and cascade of a timer are `Wheel.lean`'s, for any unit: the microsecond
wheel is the same wheel. Which backend timer ends the shard's wait at the deadline is the TLA+
model's part (`proofs/tla/precise/`).
-/

namespace Nuclei.Precise

/-- A deadline in nanoseconds, as the microsecond wheel arms it: rounded up. -/
def armed (d : Nat) : Nat := (d + 999) / 1000

/-- The clock in nanoseconds, as the microsecond wheel reads it: rounded down. -/
def reading (now : Nat) : Nat := now / 1000

/-- The same two for the millisecond wheel of `sleep`. -/
def armedTick (d : Nat) : Nat := (d + 999999) / 1000000
def readingTick (now : Nat) : Nat := now / 1000000

/-- A precise timer never fires before its deadline: once the wheel's reading reaches the
armed time, the clock is at or past the deadline. -/
theorem never_early (d now : Nat) (due : armed d ≤ reading now) : d ≤ now := by
  unfold armed reading at due
  omega

/-- Nor does a timer of the millisecond wheel. -/
theorem never_early_on_the_tick (d now : Nat) (due : armedTick d ≤ readingTick now) :
    d ≤ now := by
  unfold armedTick readingTick at due
  omega

/-- A precise timer is due as soon as the clock is one microsecond, at most, past its
deadline. -/
theorem at_most_a_microsecond_late (d now : Nat) (late : d + 999 ≤ now) :
    armed d ≤ reading now := by
  unfold armed reading
  omega

/-- A deadline on the microsecond grid is due the moment the clock reaches it: a paused clock
that jumps to it fires it exactly there. -/
theorem on_the_grid_exactly (d : Nat) (grid : d % 1000 = 0) : armed d ≤ reading d := by
  unfold armed reading
  omega

/-- The time a paused shard publishes for its timer, the armed microsecond in nanoseconds, is
never before the deadline, and less than a microsecond after it. -/
theorem published_within_a_microsecond (d : Nat) :
    d ≤ armed d * 1000 ∧ armed d * 1000 < d + 1000 := by
  unfold armed
  omega

/-- The worked example. -/
theorem example_rounding :
    armed 2000001 = 2001 ∧ armedTick 2000001 = 3 ∧ reading 2000999 = 2000 ∧
      reading 2001000 = 2001 := by
  decide

end Nuclei.Precise
