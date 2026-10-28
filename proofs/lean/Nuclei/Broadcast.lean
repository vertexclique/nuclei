/-!
# The broadcast channel's position arithmetic

This file checks the arithmetic the broadcast channel (`src/sync/broadcast.rs`) rests on: which
slot a value goes to, what a receiver may conclude when its slot holds a later value, where it
goes on from, and that what it takes plus what it hears it lost is always everything sent.

## The story in plain words

The channel is a ring of `c` slots. The value sent at position `p` goes to slot `p % c`. A
receiver waits for position `n`, its next one, in slot `n % c`. If that slot holds a later
position `p`, the value of `n` was overwritten: `p` is a whole ring (or more) past `n`, so the
receiver lost values. The newest position sent so far is below the tail `t`, and only the last
`c` positions below `t` can still be in the ring: the receiver goes on from `t - c`, which is
past `n`, and hears that it lost `t - c - n` values. Those `c` positions all have different
slots, so each can still be read.

A receiver counts what it takes and what it hears it lost. Taking `n` adds one to both its
count and its position; a lag adds `t - c - n` to its count of lost values and moves its
position to `t - c`. Either way, taken plus lost stays the number of positions it has passed.
When the channel closes and the receiver reaches the tail, that number is every value sent
after it subscribed: none goes missing without being counted.

A worked example, computed below: a ring of 4, a receiver at 2, the tail at 10. Slot `2 % 4 = 2`
holds position 6 (a ring past 2, from the send of 6). The receiver goes on from `10 - 4 = 6`,
having lost 4 values (2, 3, 4 and 5); positions 6 to 9 sit in slots 2, 3, 0 and 1.

## What the model leaves out

The concurrency (senders racing for a slot, the compare-and-swap that never puts an earlier
position over a later one, receivers reading while senders write, the close) is the TLA+
model's part, in `proofs/tla/broadcast/`. The code computes `p % c` as `p & (c - 1)` with `c` a
power of two, which a unit test checks, not this file.
-/

namespace Nuclei.Broadcast

/-- The slot of position `p` in a ring of `c` slots. -/
def slotOf (c p : Nat) : Nat := p % c

/-- Every position has a slot of the ring. -/
theorem slot_in_ring (c p : Nat) (hc : 0 < c) : slotOf c p < c :=
  Nat.mod_lt p hc

/-- A slot that holds a later position than the receiver's holds one at least a whole ring
later: two positions with the same slot differ by a multiple of `c`. -/
theorem later_in_same_slot_is_a_ring_later (c n p : Nat)
    (same : slotOf c p = slotOf c n) (later : n < p) : n + c ≤ p := by
  -- `p - n` is a positive multiple of `c`, so at least `c`.
  have zero : (p - n) % c = 0 := Nat.sub_mod_eq_zero_of_mod_eq same
  have divides : c ∣ p - n := Nat.dvd_of_mod_eq_zero zero
  have positive : 0 < p - n := Nat.sub_pos_of_lt later
  have atLeast : c ≤ p - n := Nat.le_of_dvd positive divides
  omega

/-- A lagging receiver goes on from `t - c`, which is past its position: it lost at least one
value. The slot's later position `p` was sent, so it is below the tail `t`. -/
theorem lag_goes_forward (c n p t : Nat)
    (same : slotOf c p = slotOf c n) (later : n < p) (sent : p < t) : n < t - c := by
  have ring := later_in_same_slot_is_a_ring_later c n p same later
  omega

/-- The `c` positions below the tail sit in different slots: none of them overwrote another, so
each can still be read. -/
theorem window_slots_differ (c q r t : Nat)
    (low : t - c ≤ q) (lt : q < r) (high : r < t) : slotOf c q ≠ slotOf c r := by
  intro same
  have ring := later_in_same_slot_is_a_ring_later c q r same.symm lt
  omega

/-- A receiver's state: its next position, what it took, what it heard it lost, all counted
from the position it subscribed at. -/
structure Receiver where
  start : Nat
  next : Nat
  taken : Nat
  lost : Nat

/-- Taken plus lost is the number of positions the receiver has passed. -/
def Counted (r : Receiver) : Prop := r.taken + r.lost + r.start = r.next

/-- Taking the value at `next`. -/
def take (r : Receiver) : Receiver := { r with next := r.next + 1, taken := r.taken + 1 }

/-- A lag: the receiver goes on from `t - c`, counting what it skipped. -/
def lag (r : Receiver) (c t : Nat) : Receiver :=
  { r with next := t - c, lost := r.lost + (t - c - r.next) }

theorem take_counts (r : Receiver) (h : Counted r) : Counted (take r) := by
  simp only [Counted, take] at *
  omega

/-- A lag keeps the count, as long as it goes forward (which `lag_goes_forward` shows it does). -/
theorem lag_counts (r : Receiver) (c t : Nat) (h : Counted r) (forward : r.next ≤ t - c) :
    Counted (lag r c t) := by
  simp only [Counted, lag] at *
  omega

/-- At the close, a receiver at the tail took or heard of every value sent since it subscribed. -/
theorem closed_receiver_accounts_for_every_value (r : Receiver) (t : Nat) (h : Counted r)
    (atTail : r.next = t) : r.taken + r.lost = t - r.start := by
  unfold Counted at h
  omega

-- The worked example above.
example : slotOf 4 2 = slotOf 4 6 := by decide
example : 10 - 4 = 6 ∧ 6 - 2 = 4 := by decide
example : [6, 7, 8, 9].map (slotOf 4) = [2, 3, 0, 1] := by decide

end Nuclei.Broadcast
