/-!
# A shard's parked time in one word

This file checks the encoding a shard publishes its parked time in (`src/rt/counters.rs`,
`Parked::encode` and `Parked::decode`), which a reader on any thread loads as one word.

## The story in plain words

A shard is either running or parked. Running, the word holds the time it spent parked in all.
Parked since some moment, the word holds that moment less the time parked before it, with a flag
set, and a reader turns it back into parked time with its own clock: its clock less the number
is the parks before plus this one so far. The proofs below say the reader gets exactly that, the
running word reads the same at every clock, and no read claims more parked time than the shard
has lived. The temporal side (when the shard stores the word, and reads that straddle a change)
is `proofs/tla/busy/Busy.tla`.
-/

namespace Nuclei.Busy

/-- The published word: the parked flag and a number of nanoseconds. -/
structure Word where
  -- whether the shard is parked now
  parked : Bool
  -- not parked: the time parked in all; parked: the park's start less the time parked before it
  val : Nat

/-- The word of a running shard that spent `p` parked in all. -/
def ran (p : Nat) : Word := ⟨false, p⟩

/-- The word of a shard parked since `b`, after `p` parked before. -/
def parks (b p : Nat) : Word := ⟨true, b - p⟩

/-- What a reader decodes from `w` at its clock `now`. -/
def decode (w : Word) (now : Nat) : Nat :=
  -- parked: the clock less the number; running: the number itself
  if w.parked then now - w.val else w.val

/-- A running shard's word reads as its total, whatever the reader's clock. -/
theorem decode_ran (p now : Nat) : decode (ran p) now = p := by
  -- the flag is clear, so the number itself is the answer
  simp [decode, ran]

/-- A parked shard's word reads as the parks before plus this one so far, once the reader's clock
is past the park's start, since no shard parks longer than it has lived (`p ≤ b`). -/
theorem decode_parks (b p now : Nat) (hp : p ≤ b) (hb : b ≤ now) :
    decode (parks b p) now = p + (now - b) := by
  -- the flag is set, so the word decodes to `now - (b - p)` by definition
  have h : decode (parks b p) now = now - (b - p) := rfl
  -- which is `p + (now - b)` under the two bounds
  rw [h]
  omega

/-- No read of a parked shard's word claims more parked time than the reader's clock: the busy
time a reader computes, its clock less the parked time, never goes below zero. -/
theorem decode_parks_le (b p now : Nat) : decode (parks b p) now ≤ now := by
  -- the flag is set, so the word decodes to `now - (b - p)` by definition
  have h : decode (parks b p) now = now - (b - p) := rfl
  -- which is at most `now`
  rw [h]
  omega

/-- A park that ends adds its own length to the total: the running word after a park from `b` to
`w` reads as what the parked word read at `w`. -/
theorem wake_continues (b p w : Nat) (hp : p ≤ b) (hb : b ≤ w) :
    decode (ran (p + (w - b))) w = decode (parks b p) w := by
  -- both sides are `p + (w - b)`
  rw [decode_ran, decode_parks b p w hp hb]

end Nuclei.Busy
