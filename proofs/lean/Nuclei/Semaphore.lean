/-!
# The barging semaphore's nudges

This file checks the part of the barging semaphore's drain pass that picks whom to wake
(`src/sync/raw_semaphore.rs`, `RawSemaphore::pass`): with nobody owed a handoff, the pass nudges
the oldest waiters the free permits could serve, oldest first, and stops at the first that does not
fit; after a release it nudges the front waiter whatever it asks for, so a large request at the
front still tries, loses, counts the bypass, and is owed a handoff in the end rather than starving
behind smaller callers.

## The story in plain words

The pass walks the line from the front with a budget, the free permits. A waiter whose request
fits the budget is nudged and its request taken off the budget; the first that does not fit ends
the walk, so nobody behind it is nudged, however small. After a release the front waiter is
nudged even when it does not fit, and the walk goes on with what is left (nothing, then). So the
waiters nudged are always the front of the line, never a waiter behind one passed over; their
requests add up to at most the budget, plus the front's own when the release woke it; and a
smaller request behind a large front that does not fit is never served before it by a nudge.

A worked example, computed below: free permits 3, requests `[2, 2, 1]` with no release: the first
fits (1 left), the second does not, so one is nudged and the `1` behind is not.

## What the model leaves out

Who is in the line and how a nudge reaches its waiter (the drain role, the arrival stack, the
nudge marker and the waker) is the TLA+ model's part, in `proofs/tla/semaphore/`. The budget's
`saturating_sub` is the natural numbers' subtraction here, which also stops at zero.
-/

namespace Nuclei.Semaphore

/-- How many waiters, from the front of `line` (their requests), a pass nudges with `budget`
free permits; `wakeFront`: a release asked that the front be woken whatever it asks for. -/
def nudged (budget : Nat) (wakeFront : Bool) : List Nat → Nat
  | [] => 0
  | need :: rest =>
    if need ≤ budget ∨ wakeFront = true then 1 + nudged (budget - need) false rest else 0

/-- The requests of the waiters nudged: the front `nudged` of the line. -/
def nudgedNeeds (budget : Nat) (wakeFront : Bool) (line : List Nat) : List Nat :=
  line.take (nudged budget wakeFront line)

/-- Never more nudges than waiters. -/
theorem nudged_le_length (budget : Nat) (wf : Bool) (line : List Nat) :
    nudged budget wf line ≤ line.length := by
  induction line generalizing budget wf with
  | nil => simp [nudged]
  | cons need rest ih =>
    unfold nudged
    split
    · have := ih (budget - need) false
      simp only [List.length_cons]
      omega
    · omega

/-- Without a release's request, the requests nudged add up to at most the budget. -/
theorem nudged_fit_the_budget (budget : Nat) (line : List Nat) :
    (nudgedNeeds budget false line).sum ≤ budget := by
  induction line generalizing budget with
  | nil => simp [nudgedNeeds, nudged]
  | cons need rest ih =>
    unfold nudgedNeeds nudged
    by_cases h : need ≤ budget
    · simp only [h, true_or, ite_true, Nat.add_comm 1, List.take_succ_cons, List.sum_cons]
      have := ih (budget - need)
      unfold nudgedNeeds at this
      omega
    · simp [h]

/-- With a release's request, they add up to at most the budget plus the front's own request. -/
theorem nudged_fit_with_the_front (budget need : Nat) (rest : List Nat) :
    (nudgedNeeds budget true (need :: rest)).sum ≤ budget + need := by
  unfold nudgedNeeds nudged
  simp only [or_true, ite_true, Nat.add_comm 1, List.take_succ_cons, List.sum_cons]
  have := nudged_fit_the_budget (budget - need) rest
  unfold nudgedNeeds at this
  omega

/-- A front waiter whose request does not fit, with no release's request, stops the walk: nobody
behind it is nudged, however small. -/
theorem an_unfit_front_is_not_skipped (budget need : Nat) (rest : List Nat) (big : budget < need) :
    nudged budget false (need :: rest) = 0 := by
  unfold nudged
  simp [show ¬ need ≤ budget by omega]

/-- After a release the front waiter is nudged, whatever it asks for. -/
theorem a_release_wakes_the_front (budget need : Nat) (rest : List Nat) :
    0 < nudged budget true (need :: rest) := by
  unfold nudged
  simp only [or_true, ite_true]
  omega

/-- Waiters that fit one after another, then one that does not fit what they left: the walk stops
there, nudging exactly those that fit and nobody behind, however small. -/
theorem stops_at_the_first_unfit (budget big : Nat) (pre rest : List Nat)
    (fit : pre.sum ≤ budget) (unfit : budget - pre.sum < big) :
    nudged budget false (pre ++ big :: rest) = pre.length := by
  induction pre generalizing budget with
  | nil =>
    simp only [List.nil_append, List.length_nil]
    exact an_unfit_front_is_not_skipped budget big rest (by simpa using unfit)
  | cons need more ih =>
    simp only [List.sum_cons] at fit unfit
    simp only [List.cons_append, List.length_cons]
    unfold nudged
    simp only [show need ≤ budget by omega, true_or, ite_true]
    rw [ih (budget - need) (by omega) (by omega)]
    omega

-- The worked example: free 3, requests 2, 2, 1, no release: one nudged.
example : nudged 3 false [2, 2, 1] = 1 := by decide
-- After a release the front is nudged even when nothing is free.
example : nudged 0 true [2, 1] = 1 := by decide

end Nuclei.Semaphore
