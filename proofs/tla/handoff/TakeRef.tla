------------------------------ MODULE TakeRef ------------------------------
(***************************************************************************)
(* Who holds a shard while the monitor takes it from its runner (`take` in *)
(* src/rt/handoff/monitor.rs, `Slot::take` and `Slot::come_back` in       *)
(* src/rt/handoff/slot.rs, `run_shard` in src/rt/workers.rs).              *)
(*                                                                          *)
(* Who acts. A runner runs the shard, holding one reference to it, and is  *)
(* outside the shard in a long poll. The monitor holds the shard only      *)
(* weakly: to take it, it gets a reference of its own (an upgrade, which   *)
(* fails once no reference is left), claims the runner's slot and takes it *)
(* with a compare-and-swap, then gives the shard and its reference to a    *)
(* spare thread, which runs it from then on.                               *)
(*                                                                          *)
(* What can happen. The runner's poll ends at any moment. Coming back, it  *)
(* finds no claim and goes on, cancels a pending one and goes on, or finds *)
(* the shard taken: it is detached, and lets go of its reference. A take   *)
(* that does not happen (the runner came back first) lets go of the        *)
(* monitor's reference.                                                     *)
(*                                                                          *)
(* What must never happen. The shard is freed while the runtime runs: its  *)
(* last reference gone, it drops every task still queued on it, and each   *)
(* awaits an output that never comes. And no reference is lost or kept     *)
(* past its holder. The bug this model was made for: the monitor got its   *)
(* reference after the take; a runner that came back in between found the *)
(* shard taken, let go of the last reference and freed it.                 *)
(*                                                                          *)
(* What is abstract. The claim's fence and the second read of the word are *)
(* one step (proofs/tla/handoff/Handoff.tla checks them); the runner makes *)
(* one poll; the spare is one step that takes the gift and runs for good   *)
(* (its start hook's panic is caught, tla/hooks/Hooks.tla).                *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the name of the one rule this run breaks

(* --algorithm TakeRef                                                       \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    refs = 1,              \* references to the shard: the runner's to begin with
    freed = FALSE,         \* the last reference went and the shard was dropped
    claim = "none",        \* the runner's slot: "none", "pending" or "taken"
    runnerIn = FALSE,      \* the runner came back in and runs the shard on
    runnerHolds = TRUE,    \* the runner holds its reference
    monitorHolds = FALSE,  \* the monitor holds a reference of its own
    spareHolds = FALSE,    \* a spare took the shard, with its reference
    failedTake = FALSE;    \* (record) a take that did not happen let go of its reference

define                                                                       \* names the properties use:
    \* The shard is never freed while the runtime runs.
    NoLoss == ~freed                                                         \* the shard is never dropped
    \* Each reference has its holder, and each holder its one reference.
    RefsHeld == freed \/ refs = (IF runnerHolds THEN 1 ELSE 0)               \* the runner's,
                              + (IF monitorHolds THEN 1 ELSE 0)              \* the monitor's,
                              + (IF spareHolds THEN 1 ELSE 0)                \* and the spare's
    \* A taken shard reaches a spare in the end.
    Rescued == (claim = "taken") ~> spareHolds                               \* taken, then run elsewhere
    \* Witness: the take happens.
    NoTake == claim # "taken"                                                \* claims to: never taken
    \* Witness: a take that does not happen lets go of its reference.
    NoFailedTake == ~failedTake                                              \* claims to: never a failed take
end define;                                                                  \* end of the names

\* One reference less; the last one drops the shard.
macro let_go() begin                                                         \* a holder lets go
    refs := refs - 1;                                                        \* one reference fewer
    if refs = 0 then freed := TRUE; end if;                                  \* the last one frees the shard
end macro;                                                                   \* end of the letting go

\* The runner (`run_shard` and the come-back of its poll, `Slot::come_back` and `Slot::contest`).
fair process runner = "runner"                                               \* weakly fair: its poll ends
begin                                                                        \* the runner's steps:
R_back:                                                                      \* its poll ends: it comes back
    if claim = "taken" then                                                  \* the monitor took the shard:
        goto R_drop;                                                         \* it is detached
    else                                                                     \* no claim, or a pending one:
        claim := "none";                                                     \* a pending claim is cancelled
        runnerIn := TRUE;                                                    \* and the runner goes on in
        goto Done;                                                           \* running the shard, holding it
    end if;                                                                  \* one of the two
R_drop:                                                                      \* detached: `drop(shard)` in `run_shard`
    runnerHolds := FALSE;                                                    \* its reference goes
    let_go();                                                                \* and may be the last
end process;                                                                 \* end of the runner

\* The monitor (`take`, then `give`).
fair process monitor = "monitor"                                             \* weakly fair: it acts
begin                                                                        \* the monitor's steps:
M_hold:                                                                      \* `place.shard.0.upgrade()`, first
    \* MUTATION late_ref: the reference is got after the take, as it was before the fix.
    if Mutation # "late_ref" then                                            \* held before the claim:
        if refs = 0 then goto Done;                                          \* the shard went: nothing is claimed
        else refs := refs + 1; monitorHolds := TRUE;                         \* a reference of its own
        end if;                                                              \* one of the two
    end if;                                                                  \* now the claim
M_claim:                                                                     \* `claim`, the fence, the word read again
    if runnerIn then                                                         \* the runner is no longer outside:
        goto M_fail;                                                         \* the claim is cancelled
    else                                                                     \* still outside:
        claim := "pending";                                                  \* the claim is up
    end if;                                                                  \* now the take
M_take:                                                                      \* `Slot::take`, pending to taken
    if claim = "pending" then                                                \* the runner did not cancel:
        claim := "taken";                                                    \* the shard is the monitor's
    else                                                                     \* the runner came back first:
        goto M_fail;                                                         \* nothing is taken
    end if;                                                                  \* taken
M_late:                                                                      \* where the old code held the shard
    if Mutation = "late_ref" then                                            \* only now:
        if refs = 0 then goto Done;                                          \* gone: nothing to give
        else refs := refs + 1; monitorHolds := TRUE;                         \* a reference after all
        end if;                                                              \* one of the two
    end if;                                                                  \* now the gift
M_give:                                                                      \* `give`: a spare takes the gift
    spareHolds := TRUE;                                                      \* the spare holds the reference
    monitorHolds := FALSE;                                                   \* the monitor's became the spare's
    goto Done;                                                               \* the take is over
M_fail:                                                                      \* a take that does not happen
    failedTake := TRUE;                                                      \* (recorded)
    if monitorHolds then                                                     \* with the reference it got:
        monitorHolds := FALSE;                                               \* the monitor holds it no more
        \* MUTATION keep_ref: the failed take forgets its reference without letting go of it.
        if Mutation # "keep_ref" then let_go(); end if;                      \* `let_go(runtime, shard)`
    end if;                                                                  \* the take is over
end process;                                                                 \* end of the monitor
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
