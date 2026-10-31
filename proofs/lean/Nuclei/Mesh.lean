/-!
# The mesh's layout

This file checks how `sync::mesh::mesh` (`src/sync/mesh.rs`) lays out its channels: one bounded
channel for every ordered pair of parties, the sender of channel `(from, to)` kept by party
`from` at index `to`, its receiver by party `to` at index `from`.

## The story in plain words

`mesh(n, capacity)` makes its channels in two nested loops, the sending party outside and the
receiving party inside, so channel number `from * n + to` (counting from zero) joins `from` to
`to`. Each step pushes the new sender onto party `from`'s list and the new receiver onto party
`to`'s: party `from`'s senders come out in the order of `to`, and party `to`'s receivers in the
order of `from`. So what party `i` sends to `j` on is exactly what party `j` receives from `i`
on, every channel has one sender's place and one receiver's place, and no two pairs share one.

A worked example, computed below: with 3 parties, party 1's sender to party 2 and party 2's
receiver from party 1 are both channel 5, and channel 5 joins 1 to 2.

## What the model leaves out

The channels themselves (their order, backpressure and close) are the bounded channel's, which
the `mpsc` models check; `Party::recv`'s round robin over the receivers is the TLA+ model's part
(`proofs/tla/mesh/`).
-/

namespace Nuclei.Mesh

/-- The channel made at outer step `from`, inner step `to`: the loops make `n` channels per
sending party. -/
def chan (n src dst : Nat) : Nat := src * n + dst

/-- Party `i`'s senders, in the order they were pushed: one per receiving party. -/
def senders (n i : Nat) : List Nat := (List.range n).map (fun j => chan n i j)

/-- Party `j`'s receivers, in the order they were pushed: one per sending party, as the outer
loop runs. -/
def receivers (n j : Nat) : List Nat := (List.range n).map (fun i => chan n i j)

/-- Party `i`'s sender at index `j` and party `j`'s receiver at index `i` are one channel. -/
theorem the_ends_meet (n i j : Nat) (hi : i < n) (hj : j < n) :
    (senders n i)[j]'(by simp [senders, hj]) = (receivers n j)[i]'(by simp [receivers, hi]) := by
  simp [senders, receivers]

/-- A channel joins exactly one pair: its number tells the sending and the receiving party. -/
theorem one_pair_per_channel (n i j : Nat) (hj : j < n) :
    chan n i j / n = i ∧ chan n i j % n = j := by
  have pos : 0 < n := by omega
  unfold chan
  constructor
  · rw [Nat.add_comm, Nat.add_mul_div_right j i pos, Nat.div_eq_of_lt hj]
    omega
  · rw [Nat.add_comm, Nat.add_mul_mod_self_right, Nat.mod_eq_of_lt hj]

/-- So no two pairs share a channel. -/
theorem pairs_do_not_share (n i j i' j' : Nat) (hj : j < n) (hj' : j' < n)
    (same : chan n i j = chan n i' j') : i = i' ∧ j = j' := by
  have a := one_pair_per_channel n i j hj
  have b := one_pair_per_channel n i' j' hj'
  rw [same] at a
  exact ⟨a.1.symm.trans b.1, a.2.symm.trans b.2⟩

/-- The worked example. -/
theorem example_three : chan 3 1 2 = 5 ∧ (senders 3 1)[2]! = 5 ∧ (receivers 3 2)[1]! = 5 := by
  decide

end Nuclei.Mesh
