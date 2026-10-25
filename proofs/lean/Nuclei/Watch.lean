/-!
# The watch channel's state word

This file checks the arithmetic of the watch channel's state word (`src/sync/watch.rs`): one 64-bit
word holding a version in its high bits and the closed bit in its lowest. A send adds `STEP` (two)
with wrapping, a close sets the low bit, and a receiver compares the version part with the one it
last saw.

## The story in plain words

The word starts at zero: version zero, open. A send adds two, so the low bit never changes, and the
version moves by one step, wrapping past the top of the word. The last sender to go sets the low
bit, which leaves the version as it was. A receiver remembers the version it last saw and calls the
channel changed when the version part differs from it. Every send moves the version, and after any
number of sends short of a whole turn of the word (2^63 of them) the version differs from the one a
receiver saw before them, so a receiver that slept through them still hears of a change.
`mark_changed` steps the receiver's own version back one step, which always differs from the
current version, so the next `changed` returns at once.

A worked example, computed below: from the word 4 (version 4, open), a send makes 6, a close makes 7,
whose version is still 6.

## What the model leaves out

The concurrency (a send's store, bump and wake; `changed` making its `Notified` before it reads the
word; the compare-and-swap of a modifying send) is the TLA+ model's part, in `proofs/tla/watch/`.
The code reads the version with `state & !1` and sets the bit with `state | 1`; here they are
`s - s % 2` and `s + 1` on an even `s`, which they equal for every 64-bit word (checked on samples
below). A receiver that misses a whole turn of the word (2^63 sends) would think nothing changed;
the version cannot tell, and at one send a nanosecond that takes centuries.
-/

namespace Nuclei.Watch

/-- The size of the word: values are below it. -/
def W : Nat := 18446744073709551616

/-- One send. -/
def STEP : Nat := 2

/-- The version part of a word (`state & !CLOSED`). -/
def versionOf (s : Nat) : Nat := s - s % 2

/-- Whether the word says every sender is gone. -/
def isClosed (s : Nat) : Prop := s % 2 = 1

/-- A send's bump (`fetch_add(STEP)`, wrapping). -/
def publish (s : Nat) : Nat := (s + STEP) % W

/-- The last sender's close (`fetch_or(CLOSED)`). -/
def close (s : Nat) : Nat := if s % 2 = 0 then s + 1 else s

/-- A word stays a word. -/
theorem publish_lt (s : Nat) : publish s < W := by
  unfold publish W STEP
  omega

theorem close_lt (s : Nat) (h : s < W) : close s < W := by
  unfold close W at *
  split <;> omega

/-- A send leaves the closed bit as it was. -/
theorem publish_keeps_closed (s : Nat) : publish s % 2 = s % 2 := by
  unfold publish W STEP
  omega

/-- A send moves the version by one step, wrapping. -/
theorem publish_moves_version (s : Nat) (h : s < W) :
    versionOf (publish s) = (versionOf s + STEP) % W := by
  unfold versionOf publish W STEP at *
  omega

/-- A close sets the closed bit. -/
theorem close_closes (s : Nat) : isClosed (close s) := by
  unfold isClosed close
  split <;> omega

/-- A close leaves the version as it was. -/
theorem close_keeps_version (s : Nat) : versionOf (close s) = versionOf s := by
  unfold versionOf close
  split <;> omega

/-- `k` sends from the word `s`. -/
def publishN : Nat → Nat → Nat
  | 0, s => s
  | k + 1, s => publish (publishN k s)

/-- `k` sends move the version by `k` steps, wrapping. -/
theorem publishN_version (k s : Nat) (h : s < W) :
    versionOf (publishN k s) = (versionOf s + STEP * k) % W := by
  induction k with
  | zero =>
    simp only [publishN]
    unfold versionOf W at *
    omega
  | succ k ih =>
    simp only [publishN]
    have hlt : publishN k s < W := by
      cases k with
      | zero => simpa [publishN] using h
      | succ k => simp only [publishN]; exact publish_lt _
    rw [publish_moves_version _ hlt, ih]
    unfold W STEP
    omega

/-- After any number of sends short of a whole turn of the word, the version differs from the one
before them: a receiver that slept through them hears of a change. -/
theorem sends_are_seen (k s : Nat) (h : s < W) (some : 0 < k) (short : k < W / STEP) :
    versionOf (publishN k s) ≠ versionOf s := by
  rw [publishN_version k s h]
  unfold versionOf W STEP at *
  omega

/-- `mark_changed`: the receiver's version one step back always differs from the current one. -/
theorem mark_changed_makes_a_change (seen : Nat) (h : seen < W) : (seen + W - STEP) % W ≠ seen := by
  unfold W STEP at *
  omega

-- The worked example.
example : publish 4 = 6 ∧ close 6 = 7 ∧ versionOf 7 = 6 := by decide

-- The arithmetic forms agree with the code's bit operations, on samples across the word.
example : [0, 1, 6, 7, W - 2, W - 1].map (fun s => (versionOf s, s &&& (W - 2))) =
    [(0, 0), (0, 0), (6, 6), (6, 6), (W - 2, W - 2), (W - 2, W - 2)] := by decide
example : [0, 6, W - 2].map (fun s => (close s, s ||| 1)) = [(1, 1), (7, 7), (W - 1, W - 1)] := by
  decide

end Nuclei.Watch
