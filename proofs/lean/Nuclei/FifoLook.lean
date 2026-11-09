/-!
# The looks at a FIFO on Apple systems

This file checks the timer `src/io/source/read/fifo.rs` gives a read side of a named FIFO on an
Apple system (issue #58). kqueue never reports that such a FIFO's writer closed once its reader
drained it, so a side that waits for its next edge also looks again on a timer: 1 ms after it
blocked, twice as long after each look that finds nothing, never more than 64 ms.

## The story in plain words

The waits are 1, 2, 4, 8, 16, 32, 64, 64, 64, ... milliseconds: never shorter than 1 ms, so the
side never spins, and never longer than 64 ms. The looks come at the sums of the waits. Whenever
the writer's close comes (at any time from the moment the side blocked), some look comes at or
after it, and no later than 64 ms after it: the end is found within 64 ms, and a look that comes
on the very millisecond of the close counts.

## What the model leaves out

Times are whole milliseconds from the moment the side blocked; a timer fires at its deadline (the
runtime's wheels never fire early, and a late fire only moves every later look by as much). A real
edge in between (bytes came) starts the waits again at 1 ms, which only brings looks sooner. The
look itself (one `read`) is taken to see what is there.
-/

namespace Nuclei.FifoLook

/-- The wait before look `k`, in milliseconds: 1 ms first, then twice the one before, at most
64 ms. -/
def wait : Nat → Nat
  | 0 => 1
  | k + 1 => min (2 * wait k) 64

/-- The time of look `k`, in milliseconds after the side blocked. -/
def look : Nat → Nat
  | 0 => wait 0
  | k + 1 => look k + wait (k + 1)

/-- Every wait is at least 1 ms: the side never spins. -/
theorem wait_pos (k : Nat) : 1 ≤ wait k := by
  induction k with
  | zero => simp [wait]
  | succ k ih => simp only [wait]; omega

/-- No wait is longer than 64 ms. -/
theorem wait_le (k : Nat) : wait k ≤ 64 := by
  induction k with
  | zero => simp [wait]
  | succ k _ => simp only [wait]; omega

/-- Two looks in a row are at least 1 ms and at most 64 ms apart. -/
theorem look_gap (k : Nat) : look k + 1 ≤ look (k + 1) ∧ look (k + 1) ≤ look k + 64 := by
  have h1 := wait_pos (k + 1)
  have h2 := wait_le (k + 1)
  simp only [look]
  omega

/-- Whenever the end comes, at `c` milliseconds after the side blocked, a look comes at or after
it and no later than 64 ms after it. -/
theorem end_found_within (c : Nat) : ∃ k, c ≤ look k ∧ look k ≤ c + 64 := by
  induction c with
  | zero => exact ⟨0, by simp [look, wait]⟩
  | succ c ih =>
    obtain ⟨k, hc, hk⟩ := ih
    by_cases h : c + 1 ≤ look k
    · exact ⟨k, h, by omega⟩
    · have gap := look_gap k
      exact ⟨k + 1, by omega, by omega⟩

/-- Worked example: the first looks come 1, 3, 7, 15, 31, 63, 127 and 191 ms after the side
blocked. -/
example : (List.range 8).map look = [1, 3, 7, 15, 31, 63, 127, 191] := by decide

end Nuclei.FifoLook
