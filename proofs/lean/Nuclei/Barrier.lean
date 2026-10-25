/-!
# The barrier's generations

This file checks the arithmetic the reusable barrier rests on (`src/sync/barrier.rs`): it counts
every arrival ever with one `fetch_add`, and arrival `i` (counting from zero) belongs to generation
`i / n` and leads it when `i % n` is `n - 1`. So one read-modify-write tells an arrival both its
generation and whether it completes it, whatever `n` is.

## The story in plain words

The arrivals `g * n` to `g * n + n - 1` are generation `g`: exactly `n` of them, the next `n` the
next generation, so generations follow one another without a gap or an overlap. Among a
generation's arrivals exactly one leads: the last, `g * n + n - 1`. Its arrival is the one that
makes the count `(g + 1) * n`, so when it returns its generation has every arrival, and every
waiter of that generation, which arrived before it, may go. A waiter released because the count
moved past its generation is never released early: its generation's arrivals all came.

A worked example, computed below: a barrier of three; arrivals 0, 1, 2 are generation 0 and 2
leads it; arrival 5 leads generation 1.

## What the model leaves out

The queue (a waiter queueing with its generation, the pass that reads the count after it took the
arrivals and releases every waiter of an older generation) is the TLA+ model's part, in
`proofs/tla/barrier/`. The count wraps after 2^64 arrivals, which no process reaches.
-/

namespace Nuclei.Barrier

/-- The generation of arrival `i` at a barrier of `n`. -/
def genOf (n i : Nat) : Nat := i / n

/-- Whether arrival `i` leads its generation. -/
def leads (n i : Nat) : Prop := i % n = n - 1

/-- The arrivals of generation `g` are exactly `g * n` to `g * n + n - 1`. -/
theorem gen_of_range (n g i : Nat) (hn : 0 < n) :
    genOf n i = g ↔ g * n ≤ i ∧ i < g * n + n := by
  unfold genOf
  constructor
  · intro h
    subst h
    constructor
    · exact Nat.div_mul_le_self i n
    · have := Nat.lt_div_mul_add (a := i) hn
      rw [Nat.mul_comm] at this ⊢
      omega
  · intro ⟨lo, hi⟩
    rw [Nat.div_eq_iff hn]
    exact ⟨lo, by omega⟩

/-- The last arrival of each generation leads it. -/
theorem the_last_leads (n g : Nat) (hn : 0 < n) : leads n (g * n + (n - 1)) := by
  unfold leads
  simp [Nat.add_mod, Nat.mul_mod_left, Nat.mod_eq_of_lt (show n - 1 < n by omega)]

/-- Only the last arrival of a generation leads it: one leader each. -/
theorem one_leader (n g i : Nat) (hn : 0 < n) (inGen : genOf n i = g) (lead : leads n i) :
    i = g * n + (n - 1) := by
  unfold genOf leads at *
  have _pos := hn
  have := Nat.div_add_mod i n
  rw [inGen, lead, Nat.mul_comm] at this
  omega

/-- The leader's arrival makes the count `(g + 1) * n`: its generation has every arrival. -/
theorem the_leader_completes (n g : Nat) (hn : 0 < n) :
    g * n + (n - 1) + 1 = (g + 1) * n := by
  have _pos := hn
  rw [Nat.succ_mul]
  omega

/-- A waiter released because the count moved past its generation goes only once all of that
generation's arrivals came: generation `g` is behind the count `c` only when `c ≥ (g + 1) * n`. -/
theorem released_only_when_complete (n g c : Nat) (behind : g < genOf n c) :
    (g + 1) * n ≤ c := by
  unfold genOf at behind
  have : (g + 1) * n ≤ c / n * n := Nat.mul_le_mul_right n (by omega)
  have := Nat.div_mul_le_self c n
  omega

-- The worked example.
example : (List.range 6).map (genOf 3) = [0, 0, 0, 1, 1, 1] := by decide
example : (List.range 6).filter (fun i => i % 3 = 2) = [2, 5] := by decide

end Nuclei.Barrier
