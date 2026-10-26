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

-- The broadcast channel: a position's slot is in the ring, a later position in the same slot is
-- a whole ring later, a lag goes forward, the last ring of positions sit in different slots,
-- and a receiver's taken plus lost values are everything sent since it subscribed.
#print axioms Nuclei.Broadcast.slot_in_ring
#print axioms Nuclei.Broadcast.later_in_same_slot_is_a_ring_later
#print axioms Nuclei.Broadcast.lag_goes_forward
#print axioms Nuclei.Broadcast.window_slots_differ
#print axioms Nuclei.Broadcast.take_counts
#print axioms Nuclei.Broadcast.lag_counts
#print axioms Nuclei.Broadcast.closed_receiver_accounts_for_every_value
-- The wheel: placement after the wheel's slot, inside the slot, exact on level 0, a cascade
-- that lowers the level, the next top window's wrap, and the worked example.
#print axioms Nuclei.Wheel.placed_after
#print axioms Nuclei.Wheel.inside_slot
#print axioms Nuclei.Wheel.level_zero_exact
#print axioms Nuclei.Wheel.cascade_lowers
#print axioms Nuclei.Wheel.next_window_wraps
#print axioms Nuclei.Wheel.example_seventy_thousand

-- A ring's file slots: every operation keeps a free slot unheld, unnamed and with no open copy;
-- a taken slot has no entry naming it; a free slot is never freed twice; a copy closes only
-- once unheld and unnamed; and the worked example.
#print axioms Nuclei.Files.inv_run
#print axioms Nuclei.Files.taken_slot_is_unnamed
#print axioms Nuclei.Files.no_double_free
#print axioms Nuclei.Files.copy_closes_unnamed
#print axioms Nuclei.Files.example_reuse_waits

-- A ring's overflow count: an overflow counts once however many waits it lasts, never more than
-- the looks that saw it, a set look after a clear one counts, and stretches split at a clear
-- look add up.
#print axioms Nuclei.Overflow.stutter
#print axioms Nuclei.Overflow.at_most_seen
#print axioms Nuclei.Overflow.counted_after_clear
#print axioms Nuclei.Overflow.split_at_clear

-- The watch channel's state word: a send keeps the closed bit and moves the version one step, a
-- close keeps the version, sends short of a whole turn are always seen, and `mark_changed` always
-- makes a change.
#print axioms Nuclei.Watch.publish_keeps_closed
#print axioms Nuclei.Watch.publish_moves_version
#print axioms Nuclei.Watch.close_closes
#print axioms Nuclei.Watch.close_keeps_version
#print axioms Nuclei.Watch.publishN_version
#print axioms Nuclei.Watch.sends_are_seen
#print axioms Nuclei.Watch.mark_changed_makes_a_change

-- A cancellation token's chain going: freed plus left is the chain, exactly the last holders at
-- its start are freed, a node with another handle stops the walk and is never freed, and a chain
-- of last handles of any length goes whole.
#print axioms Nuclei.Cancel.freed_and_left
#print axioms Nuclei.Cancel.frees_the_last_holders
#print axioms Nuclei.Cancel.stops_at_a_node_with_another_handle
#print axioms Nuclei.Cancel.never_frees_a_held_node
#print axioms Nuclei.Cancel.a_chain_of_last_handles_goes_whole

-- The barging semaphore's nudges: never more than the line, within the budget (plus the front's
-- request after a release), an unfit front stops the walk, a release wakes the front, and the walk
-- stops at the first waiter that does not fit what those before it left.
#print axioms Nuclei.Semaphore.nudged_le_length
#print axioms Nuclei.Semaphore.nudged_fit_the_budget
#print axioms Nuclei.Semaphore.nudged_fit_with_the_front
#print axioms Nuclei.Semaphore.an_unfit_front_is_not_skipped
#print axioms Nuclei.Semaphore.a_release_wakes_the_front
#print axioms Nuclei.Semaphore.stops_at_the_first_unfit

-- Notify: every owed wake is served or left over for the permit, no more waiters are served than
-- wait, something is left over only once every waiter was served, and a generation is never one of
-- the grant markers.
#print axioms Nuclei.Notify.served_and_left
#print axioms Nuclei.Notify.served_le_waiting
#print axioms Nuclei.Notify.left_only_when_all_served
#print axioms Nuclei.Notify.a_generation_is_never_a_marker

-- The barrier's generations: a generation is exactly n arrivals, its last leads it and only it,
-- the leader's arrival completes it, and a waiter released for a generation behind the count goes
-- only once that generation had every arrival.
#print axioms Nuclei.Barrier.gen_of_range
#print axioms Nuclei.Barrier.the_last_leads
#print axioms Nuclei.Barrier.one_leader
#print axioms Nuclei.Barrier.the_leader_completes
#print axioms Nuclei.Barrier.released_only_when_complete

-- The once-cell's pointer: a published value stays, every answer after it is that value, and from
-- an empty cell the first attempt wins.
#print axioms Nuclei.OnceCell.published_stays
#print axioms Nuclei.OnceCell.every_answer_is_the_published
#print axioms Nuclei.OnceCell.the_first_wins

-- The reader-writer lock's state word: every admitted entry, every release, a downgrade and an
-- upgrade's finish keep a writer alone with upgrading tied to the upgradable read; an upgrade
-- holds reads back, and a write needs the lock empty.
#print axioms Nuclei.RwLock.enter_keeps_good
#print axioms Nuclei.RwLock.read_release_keeps_good
#print axioms Nuclei.RwLock.write_release_keeps_good
#print axioms Nuclei.RwLock.upgradable_release_keeps_good
#print axioms Nuclei.RwLock.downgrade_keeps_good
#print axioms Nuclei.RwLock.upgrade_finish_keeps_good
#print axioms Nuclei.RwLock.upgrading_holds_reads_back
#print axioms Nuclei.RwLock.a_write_needs_the_lock_empty

-- The reentrant locks: the mutex's owner holds the permit exactly while its depth is above zero,
-- and the reader-writer lock's owner holds exactly the share its read and write depths call for,
-- after any steps; a write is refused exactly when the owner reads without writing.
#print axioms Nuclei.Reentrant.stepM_keeps_good
#print axioms Nuclei.Reentrant.the_permit_follows_the_depth
#print axioms Nuclei.Reentrant.lock_adds_one
#print axioms Nuclei.Reentrant.unlock_takes_one
#print axioms Nuclei.Reentrant.step_keeps_good
#print axioms Nuclei.Reentrant.the_share_follows_the_depths
#print axioms Nuclei.Reentrant.refused_iff

-- The waiter node's state word: a claim takes the current slot and keeps the rest, one claim at a
-- time; the register swap disarms, avoids the taken slot and keeps the marks; the arming swap
-- arms; a take claims again keeping its bits, and ends clearing only its own.
#print axioms Nuclei.Waiter.of_every
#print axioms Nuclei.Waiter.claim_takes_the_current_slot
#print axioms Nuclei.Waiter.claim_keeps_the_rest
#print axioms Nuclei.Waiter.one_claim_at_a_time
#print axioms Nuclei.Waiter.register_disarms
#print axioms Nuclei.Waiter.register_avoids_the_taken_slot
#print axioms Nuclei.Waiter.register_keeps_the_marks
#print axioms Nuclei.Waiter.arm_arms
#print axioms Nuclei.Waiter.claiming_again_keeps_the_take
#print axioms Nuclei.Waiter.a_take_ends_cleanly
