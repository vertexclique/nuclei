import Nuclei

/-!
# The axioms the proofs rest on

Lean checks every proof step, but a proof may also lean on an axiom: a fact taken without
proof. `#print axioms` lists every axiom a theorem depends on, through every lemma it uses.
`check.sh axioms` compiles this file and fails unless each list holds only Lean's standard
axioms (`propext`, `Quot.sound`, `Classical.choice`). An unfinished proof would show up here as
`sorryAx`, and a proof by running compiled code as `Lean.ofReduceBool`.

This file is not part of the library, so `lake build` does not print these lists every time.
-/

open Nuclei.FairSemaphore

-- Every permit is free, set aside or held, after any sequence of operations.
#print axioms conservation
-- While anybody waits, no permit is free.
#print axioms waiting_means_nothing_free
-- The line is in arrival order.
#print axioms queue_in_arrival_order
-- No waiter is granted ahead of an earlier one that still waits.
#print axioms no_overtaking
-- A request granted without waiting found the line empty (or asked for nothing).
#print axioms fresh_only_when_nobody_waits
-- A dropped front waiter's share goes to the next one.
#print axioms cancel_passes_share_on
-- After the close, nothing is granted again.
#print axioms closed_admits_nothing
-- The worked example, computed.
#print axioms example_after_release
#print axioms example_share_held
#print axioms example_after_cancel
