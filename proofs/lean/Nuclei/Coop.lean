/-!
# The cooperative budget's count

This file checks the counter of `src/coop.rs`: how many operations one poll of a task
completes before it yields, and that outside a budgeted poll nothing ever yields.

## The story in plain words

Each poll of a task starts with a budget, `BUDGET` (128) in the code. Every operation that
completes (an IO call, a receive, an acquire, a lock) spends one; when none is left the next
operation completes nothing and the task yields, so the poll ends. Outside a budgeted poll,
and in an unconstrained one, the counter starts at `2^64 - 1`, which no thread can spend.

A worked example, computed below: a poll with a budget of 2 and a program of 5 operations
completes 2 of them and yields at the third, with nothing left; with a budget of 128 a program
of 5 completes all 5 and leaves 123.

## What the model leaves out

The counter is one natural number per poll; saving and restoring it around a nested
`block_on`, the wake of a task that yields and the run queue are the TLA+ model's part
(`proofs/tla/coop/`). An IO operation that does not complete gets its unit back; here every
operation is one that completes when the budget lets it.
-/

namespace Nuclei.Coop

/-- The budget outside a budgeted poll: `u64::MAX` in the code. -/
def unbudgeted : Nat := 2 ^ 64 - 1

/-- One spend (`coop::try_spend`): `some` of the budget left when the operation may complete,
`none` when the budget is spent and nothing changes. -/
def spend (left : Nat) : Option Nat :=
  if left = 0 then none else some (left - 1)

/-- A poll with budget `b` that has `k` operations to do: how many complete before the first
refusal (or the end of the program), and the budget left then. -/
def run : Nat → Nat → Nat × Nat
  | b, 0 => (0, b)
  | b, k + 1 => if b = 0 then (0, b) else ((run (b - 1) k).1 + 1, (run (b - 1) k).2)

/-- A spend refuses exactly when the budget is spent. -/
theorem spend_refuses_only_when_spent (left : Nat) : spend left = none ↔ left = 0 := by
  unfold spend
  by_cases h : left = 0
  · rw [ite_eq_left h]
    simp [h]
  · rw [ite_eq_right h]
    simp [h]

/-- A spend that lets an operation complete takes exactly one, and a refund (an IO operation
that answered `Pending` after all) puts it back. -/
theorem spend_takes_one (left rest : Nat) (h : spend left = some rest) : rest + 1 = left := by
  unfold spend at h
  by_cases z : left = 0
  · rw [ite_eq_left z] at h
    exact absurd h (by simp)
  · rw [ite_eq_right z] at h
    simp only [Option.some.injEq] at h
    omega

/-- A poll never completes more operations than its budget. -/
theorem completes_at_most_the_budget (b k : Nat) : (run b k).1 ≤ b := by
  induction k generalizing b with
  | zero => simp [run]
  | succ k ih =>
    by_cases z : b = 0
    · simp [run, z]
    · have := ih (b - 1)
      simp only [run]
      rw [ite_eq_right z]
      simp only
      omega

/-- Nor more than it has to do. -/
theorem completes_at_most_the_program (b k : Nat) : (run b k).1 ≤ k := by
  induction k generalizing b with
  | zero => simp [run]
  | succ k ih =>
    by_cases z : b = 0
    · simp [run, z]
    · have := ih (b - 1)
      simp only [run]
      rw [ite_eq_right z]
      simp only
      omega

/-- A program that fits the budget completes whole, and spends one unit per operation. -/
theorem a_program_within_the_budget_completes (b k : Nat) (fits : k ≤ b) :
    (run b k).1 = k ∧ (run b k).2 = b - k := by
  induction k generalizing b with
  | zero => simp [run]
  | succ k ih =>
    have z : ¬ b = 0 := by omega
    have := ih (b - 1) (by omega)
    simp only [run]
    rw [ite_eq_right z]
    simp only
    omega

/-- A program longer than the budget completes exactly the budget, then yields with nothing
left: the other tasks of the shard run before the rest of it. -/
theorem a_longer_program_yields_after_the_budget (b k : Nat) (over : b < k) :
    (run b k).1 = b ∧ (run b k).2 = 0 := by
  induction b generalizing k with
  | zero =>
    cases k with
    | zero => omega
    | succ k => simp [run]
  | succ b ih =>
    cases k with
    | zero => omega
    | succ k =>
      have := ih k (by omega)
      simp only [run]
      rw [ite_eq_right (by omega : ¬ (b + 1 = 0)), Nat.add_sub_cancel]
      simp only
      omega

/-- Outside a budgeted poll nothing yields: every program a thread could run completes whole. -/
theorem nothing_yields_unbudgeted (k : Nat) (h : k ≤ unbudgeted) : (run unbudgeted k).1 = k :=
  (a_program_within_the_budget_completes unbudgeted k h).1

/-- The worked examples. -/
theorem example_budget_two : run 2 5 = (2, 0) := by decide
theorem example_budget_128 : run 128 5 = (5, 123) := by decide

end Nuclei.Coop
