/-!
# The remote wake handshake's order

This file checks the two facts the remote wake handshake (`src/rt/handshake.rs`) takes from the
one total order of sequentially consistent accesses: in that order one of the pair's loads comes
after the other side's store, so no wake is lost; and of a burst of wakers' swaps of the flag,
the first finds it up and every later one finds it down, so a burst notifies once.

## The story in plain words

The shard stores that it parks and then loads the inbox; a waker stores its work and then loads
the flag. Put the four accesses in one order with each side's store before its own load. If the
shard's load came before the waker's store and the waker's load before the shard's store, the
order would run in a circle: the shard's store, its load, the waker's store, the waker's load,
and back to the shard's store. So one load comes after the other side's store and sees it. A
swap reads the flag and writes it down in one step, so of the swaps in the order only the first
reads it up.

A worked example, computed below: three swaps of a flag that is up answer up, down, down.

## What the model leaves out

The store buffer, which lets a store pass a later load when the pair is relaxed with no fence, is
the TLA+ model's part, in `proofs/tla/handshake/`, which also shows that a fence on one side only
does not stop it.
-/

namespace Nuclei.Handshake

/-- In one total order with each side's store before its own load, one load comes after the
other side's store. -/
theorem one_side_sees (shardStore shardLoad wakerStore wakerLoad : Nat)
    (shard : shardStore < shardLoad) (waker : wakerStore < wakerLoad) :
    wakerStore < shardLoad ∨ shardStore < wakerLoad := by
  omega

/-- The swaps of a burst in order, from the flag `up`: what each one reads. -/
def swaps : Bool → Nat → List Bool
  | _, 0 => []
  | up, n + 1 => up :: swaps false n

theorem later_swaps_read_down (n : Nat) : (swaps false n).count true = 0 := by
  induction n with
  | zero => rfl
  | succ n ih => simpa [swaps] using ih

/-- Of a burst of swaps of a raised flag, exactly one reads it up and notifies. -/
theorem a_burst_notifies_once (n : Nat) : (swaps true (n + 1)).count true = 1 := by
  simp [swaps, later_swaps_read_down]

-- The worked example: three swaps of a raised flag.
example : swaps true 3 = [true, false, false] := by decide

end Nuclei.Handshake
