/-!
# Freeing a chain of cancellation tokens

This file checks how a cancellation token's node is freed (`src/sync/cancellation/node.rs`,
`impl Drop for Node`): a node whose last strong handle goes leaves its parent's registry and lets
go of its parent's handle, which may free the parent in turn, in a loop up the chain of
ancestors rather than by recursion, so a chain of any depth can go without the stack growing.

## The story in plain words

Take the chain from the node whose handle goes up to the root, each node with the number of strong
handles it has (its tokens, its children's handles of it, and the handles a cancel's walk holds).
Letting go of one handle of the first node takes one off its count. If that was its last, it is
freed, and its own handle of its parent goes: the next node loses one, and so on. So exactly the
nodes from the start of the chain that each held only that one handle are freed, the first node
after them (if any) loses one handle and stays, and every node above it is untouched. No node that
still has another handle is ever freed, however long the chain.

A worked example, computed below: the chain `[1, 1, 3, 1]` (a node and its parent each with their
last handle, a grandparent with three) frees two nodes and leaves `[2, 1]`.

## What the model leaves out

The concurrency (a cancel's walk upgrading a child while the child goes, the registry's insert and
removal racing a walk) is the TLA+ model's part, in `proofs/tla/cancel/`. A count here is the
`Arc`'s strong count, whose atomic decrement and `into_inner` are std's.
-/

namespace Nuclei.Cancel

/-- Lets go of one handle of the first node of `chain` (its handle counts, from the node up to the
root): how many nodes it frees, and the counts of the chain above them. -/
def release : List Nat → Nat × List Nat
  | [] => (0, [])
  | c :: rest =>
    if c ≤ 1 then
      let up := release rest
      (up.1 + 1, up.2)
    else
      (0, (c - 1) :: rest)

/-- Every node freed, plus every node left, is the whole chain. -/
theorem freed_and_left (chain : List Nat) :
    (release chain).1 + (release chain).2.length = chain.length := by
  induction chain with
  | nil => simp [release]
  | cons c rest ih =>
    unfold release
    split
    · simp only [List.length_cons]
      omega
    · simp

/-- The nodes freed are exactly the ones at the start of the chain that held their last handle. -/
theorem frees_the_last_holders (chain : List Nat) :
    (release chain).1 = (chain.takeWhile (· ≤ 1)).length := by
  induction chain with
  | nil => simp [release]
  | cons c rest ih =>
    unfold release
    by_cases h : c ≤ 1
    · simp [h, ih]
    · simp [h]

/-- A node with another handle stops the walk: it loses one and stays, with the chain above it
untouched. -/
theorem stops_at_a_node_with_another_handle (c : Nat) (rest : List Nat) (h : 1 < c) :
    release (c :: rest) = (0, (c - 1) :: rest) := by
  unfold release
  simp [show ¬ c ≤ 1 by omega]

/-- No node that has another handle is freed: if the `i`-th node of the chain has more than one,
at most `i` nodes are freed. -/
theorem never_frees_a_held_node (chain : List Nat) (i : Nat) (hi : i < chain.length)
    (held : 1 < chain[i]) : (release chain).1 ≤ i := by
  induction chain generalizing i with
  | nil => simp at hi
  | cons c rest ih =>
    unfold release
    by_cases h : c ≤ 1
    · simp only [h, ite_true]
      cases i with
      | zero => simp at held; omega
      | succ j =>
        have := ih j (by simpa using hi) (by simpa using held)
        omega
    · simp [h]

-- The worked example.
example : release [1, 1, 3, 1] = (2, [2, 1]) := by decide

/-- A chain of any length whose nodes each hold only the handle below them goes whole. -/
theorem a_chain_of_last_handles_goes_whole (n : Nat) :
    release (List.replicate n 1) = (n, []) := by
  induction n with
  | zero => simp [release]
  | succ n ih => simp [List.replicate_succ, release, ih]

end Nuclei.Cancel
