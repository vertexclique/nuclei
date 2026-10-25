/-!
# The once-cell's pointer

This file checks the state machine of the once-cell's value pointer (`src/sync/once_cell.rs`): null
while the cell is empty, then, by one compare-and-swap from null, a marker (the value in the cell's
own slot) or a box (a racing builder's value). Every way to fill the cell publishes with that one
compare-and-swap, so the pointer changes at most once, and every read after it answers the same
value.

## The story in plain words

The pointer starts null. A publication is a compare-and-swap that expects null: it succeeds once,
and every later one, from an initializer, `set` or a racing builder, fails and answers the value
already there. So whatever sequence of attempts the threads make, the pointer is null until the
first success and holds that success's value from then on, and every answer given after the first
success is that value.

A worked example, computed below: attempts by an initializer, then a racing builder, then `set`:
the initializer wins, and all three answer its value.

## What the model leaves out

The turn to write the slot, the close that ends the waits for it, and the interleaving of looks,
runs and publications are the TLA+ model's part, in `proofs/tla/oncecell/`. Taking the value out
again (`take`, `into_inner`) needs `&mut self`, so no other thread can look meanwhile.
-/

namespace Nuclei.OnceCell

/-- The pointer: empty, or holding the value a publication put there. -/
abbrev Ptr := Option Nat

/-- One publication attempt of `v`: a compare-and-swap that expects null. Returns the new pointer
and the value the caller answers. -/
def attempt (p : Ptr) (v : Nat) : Ptr × Nat :=
  match p with
  | none => (some v, v)
  | some w => (some w, w)

/-- A sequence of attempts from an empty cell: the pointer after them and every answer, in order. -/
def run : Ptr → List Nat → Ptr × List Nat
  | p, [] => (p, [])
  | p, v :: rest =>
    let (p', a) := attempt p v
    let (q, answers) := run p' rest
    (q, a :: answers)

/-- Once the pointer holds a value, no attempt changes it. -/
theorem published_stays (w : Nat) (vs : List Nat) : (run (some w) vs).1 = some w := by
  induction vs with
  | nil => rfl
  | cons v rest ih => simpa [run, attempt] using ih

/-- Every answer after a publication is the published value. -/
theorem every_answer_is_the_published (w : Nat) (vs : List Nat) :
    ∀ a ∈ (run (some w) vs).2, a = w := by
  induction vs with
  | nil => simp [run]
  | cons v rest ih =>
    intro a ha
    simp only [run, attempt, List.mem_cons] at ha
    rcases ha with h | h
    · exact h
    · exact ih a h

/-- From an empty cell, the first attempt wins: the pointer ends holding its value, and every
answer is that value. -/
theorem the_first_wins (v : Nat) (vs : List Nat) :
    (run none (v :: vs)).1 = some v ∧ ∀ a ∈ (run none (v :: vs)).2, a = v := by
  constructor
  · simpa [run, attempt] using published_stays v vs
  · intro a ha
    simp only [run, attempt, List.mem_cons] at ha
    rcases ha with h | h
    · exact h
    · exact every_answer_is_the_published v vs a h

-- The worked example: an initializer's 1, a racing builder's 2, a set's 3.
example : run none [1, 2, 3] = (some 1, [1, 1, 1]) := by decide

end Nuclei.OnceCell
