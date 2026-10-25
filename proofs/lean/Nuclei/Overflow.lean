/-!
# Counting completion-queue overflows

This file checks how a ring's poller counts overflows (`src/rt/park/ring.rs`,
`look_for_overflow`): the times the kernel held completions the ring's completion queue had no
room for. The poller looks at the kernel's overflow flag before each wait's enter and again
after it took the wait's completions, and counts a look that finds the flag set when the look
before it found it clear.

## The story in plain words

Each look sees the flag set or clear. An overflow is a run of looks that see it set: it counts
once, at its first look, however many looks (and so waits) it lasts. A look that sees the flag
clear ends the run, so the next look that sees it set begins another overflow and counts again.
The count is never more than the looks that saw the flag set, and the counts of two stretches of
looks add up when the first ends with a clear look.

The look after the completions are taken is what keeps two overflows apart: a wait that took the
last held completions clears the flag, and only a look after it sees that. Without it the next
flood's first look follows a look that saw the flag set, and the two overflows count as one (the
poller's first version did this, and its test caught it; see the witness at the end).

A worked example, computed below: looks that see set, set, clear, set, set, set count 2.

## What the model leaves out

An overflow that begins and ends between two looks is not seen, by the code or here: the counter
counts the overflows the looks saw. The flag itself (set while the kernel holds completions,
cleared as an enter moves the last of them into the queue) is the kernel's, taken as given.
-/

namespace Nuclei.Overflow

/-- The count of the looks `seen`, the look before them having seen `before`. -/
def count : Bool → List Bool → Nat
  | _, [] => 0
  | before, now :: rest => (if now && !before then 1 else 0) + count now rest

/-- A look that sees what the look before it saw adds nothing: an overflow counts once, however
many waits it lasts. -/
theorem stutter (before now : Bool) (rest : List Bool) :
    count before (now :: now :: rest) = count before (now :: rest) := by
  cases before <;> cases now <;> simp [count]

/-- The count is at most the looks that saw the flag set. -/
theorem at_most_seen (before : Bool) (seen : List Bool) :
    count before seen ≤ (seen.filter id).length := by
  induction seen generalizing before with
  | nil => simp [count]
  | cons now rest ih =>
    have := ih now
    cases before <;> cases now <;> simp [count, List.filter] <;> omega

/-- A look that sees the flag set after one that saw it clear counts. -/
theorem counted_after_clear (rest : List Bool) :
    count false (true :: rest) = 1 + count true rest := by
  simp [count]

/-- Two stretches of looks add up when the first ends with a clear look. -/
theorem split_at_clear (before : Bool) (first rest : List Bool) :
    count before (first ++ false :: rest) = count before (first ++ [false]) + count false rest := by
  induction first generalizing before with
  | nil => simp [count]
  | cons now more ih =>
    simp only [List.cons_append, count]
    rw [ih now]
    omega

-- The worked example.
example : count false [true, true, false, true, true, true] = 2 := by decide

-- The witness of the look after the completions: with it, two floods count two; without it
-- (the clear look missing), they count one.
example : count false [true, false, true] = 2 := by decide
example : count false [true, true] = 1 := by decide

/-- The mutation: counting every look that sees the flag set. It counts an overflow once per
wait it lasts, which `stutter` rules out. -/
def countEvery : Bool → List Bool → Nat
  | _, [] => 0
  | _, now :: rest => (if now then 1 else 0) + countEvery now rest

example : countEvery false [true, true] ≠ countEvery false [true] := by decide

end Nuclei.Overflow
