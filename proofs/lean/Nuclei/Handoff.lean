/-!
# The handoff's word and its watch

This file checks two pieces of the shard handoff (`src/rt/handoff/`): how a runner's word packs
its poll's epoch and its side of the shard (`slot.rs`), and what the monitor's reads of the word
say about how long a poll has run (`monitor.rs`). The protocol itself (the claim, the fences, the
take) is the TLA+ model's, `proofs/tla/handoff`.

## The story in plain words

A runner writes `2e + 1` while the task's code of its poll of epoch `e` runs (outside the
shard), and `2e` while the runtime's own code runs (inside it); each poll it starts has a new
epoch. So the word says which side the runner is on and which poll it is in, and a new poll
always writes a word no earlier poll wrote: two reads of the same outside word are two reads of
one poll. The word fits in 64 bits for every epoch below `2^63`.

The monitor reads each runner's word once a tick, the reads at least a tick apart (it sleeps to a
deadline, so a park that ends early does not shorten a tick). A read equal to the one before it,
outside, adds one to a streak; any other read ends the streak. A streak of four is five reads of
one poll, the first and the last at least four ticks apart: the poll has run at least four ticks
(the threshold) when the shard is taken. And a read that did not see the poll, just before the
first that did, says the poll began after it.

## What the model leaves out

Times are natural numbers of any unit. The model takes a poll as an interval of time in which its
word is in place (the runner's stores reach the monitor in order, which the store buffer of the
TLA+ model gives); the epoch counter is the runner's own, never shared.
-/

namespace Nuclei.Handoff

/-- The word of a runner outside its shard, in the poll of epoch `e`. -/
def outside (e : Nat) : Nat := 2 * e + 1

/-- The word of a runner inside its shard, in the poll of epoch `e`. -/
def inside (e : Nat) : Nat := 2 * e

/-- Whether a word says its runner is outside the shard. -/
def isOutside (w : Nat) : Bool := w % 2 == 1

/-- The epoch a word names. -/
def epoch (w : Nat) : Nat := w / 2

theorem epoch_outside (e : Nat) : epoch (outside e) = e := by
  unfold epoch outside; omega

theorem epoch_inside (e : Nat) : epoch (inside e) = e := by
  unfold epoch inside; omega

theorem isOutside_outside (e : Nat) : isOutside (outside e) = true := by
  unfold isOutside outside
  have : (2 * e + 1) % 2 = 1 := by omega
  simp [this]

theorem isOutside_inside (e : Nat) : isOutside (inside e) = false := by
  unfold isOutside inside
  have : (2 * e) % 2 = 0 := by omega
  simp [this]

/-- No word is both. -/
theorem outside_ne_inside (e e' : Nat) : outside e ≠ inside e' := by
  unfold outside inside; omega

/-- A new poll writes a new word: equal outside words are one epoch. -/
theorem outside_inj {e e' : Nat} (h : outside e = outside e') : e = e' := by
  unfold outside at h; omega

/-- The word fits in 64 bits for every epoch below `2^63`. -/
theorem outside_fits (e : Nat) (h : e < 2 ^ 63) : outside e < 2 ^ 64 := by
  unfold outside; omega

/-- The monitor's streak after reads `rs`, the latest first: one more for a read equal to the one
before it while outside, none after any other read. -/
def streak : List Nat → Nat
  | r :: prev :: older => if isOutside r && r == prev then streak (prev :: older) + 1 else 0
  | _ => 0

/-- A streak of `n + 1` begins with two equal outside reads, and the rest has a streak of `n`. -/
theorem streak_succ {r prev : Nat} {older : List Nat} {n : Nat}
    (h : streak (r :: prev :: older) = n + 1) :
    isOutside r = true ∧ r = prev ∧ streak (prev :: older) = n := by
  unfold streak at h
  by_cases c : (isOutside r && r == prev) = true
  · simp only [c, ite_true] at h
    simp only [Bool.and_eq_true, beq_iff_eq] at c
    exact ⟨c.1, c.2, by omega⟩
  · simp only [c] at h
    simp at h

/-- A streak of four is five equal reads, outside: what makes the monitor take a shard. -/
theorem four_is_five_reads {rs : List Nat} (h : streak rs = 4) :
    ∃ w rest, rs = w :: w :: w :: w :: w :: rest ∧ isOutside w = true := by
  match rs, h with
  | r :: p :: older, h =>
    obtain ⟨o, e1, h3⟩ := streak_succ h
    match p, older, h3 with
    | p, q :: older, h3 =>
      obtain ⟨_, e2, h2⟩ := streak_succ h3
      match q, older, h2 with
      | q, u :: older, h2 =>
        obtain ⟨_, e3, h1⟩ := streak_succ h2
        match u, older, h1 with
        | u, v :: older, h1 =>
          obtain ⟨_, e4, _⟩ := streak_succ h1
          subst e1; subst e2; subst e3; subst e4
          exact ⟨r, older, rfl, o⟩
        | _, [], h1 => simp [streak] at h1
      | _, [], h2 => simp [streak] at h2
    | _, [], h3 => simp [streak] at h3
  | [r], h => simp [streak] at h
  | [], h => simp [streak] at h

/-- The monitor's reads: read `i` at time `τ i`, each at least a tick `t` after the one before. -/
structure Reads where
  t : Nat
  τ : Nat → Nat
  spaced : ∀ i, τ i + t ≤ τ (i + 1)

/-- Read `i` sees the poll that runs over `[s, f)`. -/
def Reads.sees (rd : Reads) (s f i : Nat) : Prop := s ≤ rd.τ i ∧ rd.τ i < f

/-- Five reads in a row that see one poll: it has run at least four ticks by the last. -/
theorem ran_four_ticks (rd : Reads) {s f i : Nat} (first : rd.sees s f i)
    (last : rd.sees s f (i + 4)) : 4 * rd.t ≤ rd.τ (i + 4) - s := by
  have h0 : rd.τ i + rd.t ≤ rd.τ (i + 1) := rd.spaced i
  have h1 : rd.τ (i + 1) + rd.t ≤ rd.τ (i + 2) := rd.spaced (i + 1)
  have h2 : rd.τ (i + 2) + rd.t ≤ rd.τ (i + 3) := rd.spaced (i + 2)
  have h3 : rd.τ (i + 3) + rd.t ≤ rd.τ (i + 4) := rd.spaced (i + 3)
  unfold Reads.sees at first last
  omega

/-- A read that missed the poll, just before one that saw it: the poll began after it. -/
theorem began_after (rd : Reads) {s f i : Nat} (missed : ¬ rd.sees s f i)
    (seen : rd.sees s f (i + 1)) : rd.τ i < s := by
  have h := rd.spaced i
  unfold Reads.sees at missed seen
  omega

/-- Worked example: the words of a poll read five times after a read inside make a streak of
four. -/
example : streak [outside 3, outside 3, outside 3, outside 3, outside 3, inside 3] = 4 := by
  decide

end Nuclei.Handoff
