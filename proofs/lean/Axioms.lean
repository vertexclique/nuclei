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

-- A fresh one-shot channel is good.
#print axioms Nuclei.Oneshot.fresh_good
-- One whole operation keeps every fact of the one-shot story.
#print axioms Nuclei.Oneshot.step_good
-- So does any sequence of operations, from a fresh channel.
#print axioms Nuclei.Oneshot.run_good
-- The worked examples, computed: pending then taken; a close that owns the value; a
-- refused send that takes its value back.
#print axioms Nuclei.Oneshot.example_pending_then_taken
#print axioms Nuclei.Oneshot.example_close_owns_the_value
#print axioms Nuclei.Oneshot.example_refused_send_takes_it_back

-- A spawned task is good, every operation keeps it good, and so does any sequence.
#print axioms Nuclei.Task.spawned_good
#print axioms Nuclei.Task.step_good
#print axioms Nuclei.Task.run_good
-- The future is dropped at most once; the output is used at most once.
#print axioms Nuclei.Task.future_dropped_once
#print axioms Nuclei.Task.output_used_once
-- The memory is freed at most once, after the future and the output are gone.
#print axioms Nuclei.Task.freed_once_and_last
-- Freed memory is never touched again.
#print axioms Nuclei.Task.freed_is_inert
-- A task nobody refers to or holds a handle of is freed.
#print axioms Nuclei.Task.no_leak
-- The worked example, computed.
#print axioms Nuclei.Task.example_detached_last_waker

-- The wheel: placement after the wheel's slot, inside the slot, exact on level 0, a cascade
-- that lowers the level, the next top window's wrap, and the worked example.
#print axioms Nuclei.Wheel.placed_after
#print axioms Nuclei.Wheel.inside_slot
#print axioms Nuclei.Wheel.level_zero_exact
#print axioms Nuclei.Wheel.cascade_lowers
#print axioms Nuclei.Wheel.next_window_wraps
#print axioms Nuclei.Wheel.example_seventy_thousand
