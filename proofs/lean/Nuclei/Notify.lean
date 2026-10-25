/-!
# Notify's owed wakes and its generation

This file checks two pieces of arithmetic `Notify` rests on (`src/sync/notify.rs`): how a drain
pass serves the `notify_one` calls it owes, and why a waiter's generation can never be mistaken
for the marker a `notify_one` or `notify_last` grant leaves on a node.

## The story in plain words

A pass owes some wakes (the `notify_one` calls since the last pass, plus one for a stored permit
when somebody waits). It walks the line from the front: a waiter still waiting is granted and
takes one owed wake; one that withdrew is let go and takes none. When the owed wakes run out it
stops; when the line runs out first, what is left over becomes the stored permit again. So every
owed wake is either served or left over for the permit, none is served twice, no more waiters are
served than wait, and something is left over only once every waiter still waiting was served.

A waiter's node carries the generation it was made in; a grant from `notify_one` or `notify_last`
replaces it with one of two markers at the top of the word. Generations are taken with the word's
top bit masked off, so a generation is always below half the word, and the markers are not.

A worked example, computed below: three owed wakes and a line of waiting, withdrawn, waiting
serves two and leaves one over, which becomes the permit.

## What the model leaves out

Who is in the line, the drain role, and how a grant reaches its waiter are the TLA+ model's part,
in `proofs/tla/notify/`. The stored permit is one bit: two wakes left over store one permit,
which is what `notify_one` promises (at most one permit is stored).
-/

namespace Nuclei.Notify

/-- Serves `owed` wakes from the front of `line` (`true`: still waiting, `false`: withdrew): how
many waiters it served, and how many owed wakes are left over. -/
def serve : Nat → List Bool → Nat × Nat
  | owed, [] => (0, owed)
  | 0, _ => (0, 0)
  | owed + 1, waiting :: rest =>
    if waiting then
      let r := serve owed rest
      (r.1 + 1, r.2)
    else
      serve (owed + 1) rest

/-- Every owed wake is served or left over: none is lost, none counted twice. -/
theorem served_and_left (owed : Nat) (line : List Bool) :
    (serve owed line).1 + (serve owed line).2 = owed := by
  induction line generalizing owed with
  | nil => simp [serve]
  | cons waiting rest ih =>
    cases owed with
    | zero => simp [serve]
    | succ k =>
      unfold serve
      split
      · have := ih k
        simp only
        omega
      · exact ih (k + 1)

/-- No more waiters are served than wait. -/
theorem served_le_waiting (owed : Nat) (line : List Bool) :
    (serve owed line).1 ≤ (line.filter id).length := by
  induction line generalizing owed with
  | nil => simp [serve]
  | cons waiting rest ih =>
    cases owed with
    | zero => simp [serve]
    | succ k =>
      unfold serve
      split
      · next h =>
        have := ih k
        simp [h]
        omega
      · next h =>
        have := ih (k + 1)
        simp [h]
        omega

/-- Something is left over only once every waiter still waiting was served. -/
theorem left_only_when_all_served (owed : Nat) (line : List Bool)
    (left : 0 < (serve owed line).2) : (serve owed line).1 = (line.filter id).length := by
  induction line generalizing owed with
  | nil => simp [serve]
  | cons waiting rest ih =>
    cases owed with
    | zero => simp [serve] at left
    | succ k =>
      unfold serve at left ⊢
      split
      · next h =>
        simp only [h] at left ⊢
        have := ih k left
        simp
        omega
      · next h =>
        simp only [h] at left ⊢
        have := ih (k + 1) left
        simp [this]

/-- The size of the word. -/
def W : Nat := 18446744073709551616

/-- The markers a `notify_one` and a `notify_last` grant leave (`usize::MAX`, `usize::MAX - 1`). -/
def byFirst : Nat := W - 1
def byLast : Nat := W - 2

/-- A generation as a node carries it: the counter with the top bit masked off. -/
def masked (g : Nat) : Nat := g % (W / 2)

/-- A generation is never one of the markers, whatever the counter. -/
theorem a_generation_is_never_a_marker (g : Nat) : masked g ≠ byFirst ∧ masked g ≠ byLast := by
  have : masked g < W / 2 := Nat.mod_lt g (by decide)
  unfold byFirst byLast W at *
  omega

-- The worked example: three owed, a line of waiting, withdrawn, waiting.
example : serve 3 [true, false, true] = (2, 1) := by decide

end Nuclei.Notify
