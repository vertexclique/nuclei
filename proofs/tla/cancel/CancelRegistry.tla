---------------------------- MODULE CancelRegistry ----------------------------
(***************************************************************************)
(* A cancel's walk of a parent's registry while two children are made on  *)
(* that parent at once (src/sync/cancellation.rs: `child_token`, `cancel`, *)
(* the registry walk of src/sync/cancellation/node.rs).                    *)
(*                                                                          *)
(* In plain words: `child_token` inserts the new child into the parent's   *)
(* registry, then reads the parent's flag and cancels itself if it is set. *)
(* `cancel` sets the flag, then walks the registry. The walk is many steps *)
(* (kovan's map), not one: an entry present for the whole walk is yielded; *)
(* one inserted while the walk runs may be yielded or not. Either way a    *)
(* child inserted after the walk began read the flag after it was set, so *)
(* it cancels itself.                                                      *)
(*                                                                          *)
(* Who acts: two makers of children of one parent, and the canceller of   *)
(* the parent. What must never happen: a child left uncancelled once the  *)
(* parent's cancel and both makers are done.                               *)
(*                                                                          *)
(* What is abstract: the registry is a set and the walk yields each entry  *)
(* present when it began, plus any inserted since at the model's choice;  *)
(* the rest of the tree (grandchildren, frees, upgrades) is Cancel.tla's.  *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Mutation   \* "none", or the name of the one rule this run takes away

Makers == {"k1", "k2"}
CancelT == "x"

(* --algorithm CancelRegistry
variables
    flag = FALSE,                       \* the parent's cancelled flag
    registry = {},                      \* the parent's registry: its children
    cancelled = [m \in Makers |-> FALSE], \* each child's own flag
    walkStart = {};                     \* (the walk) the entries present as it began

define
    \* Once everyone is done, every child of a cancelled parent is cancelled.
    AllChildrenCancelled ==
        (\A p \in Makers \cup {CancelT} : pc[p] = "Done") => \A m \in Makers : cancelled[m]
end define;

fair process maker \in Makers
begin
M_insert:
    \* The new child goes into the parent's registry ...
    registry := registry \cup {self};
M_read:
    \* ... then the parent's flag is read: set, the child cancels itself.
    if flag then
        cancelled[self] := TRUE;
    end if;
end process;

fair process canceller = CancelT
begin
C_flag:
    \* `cancel`: the flag first ...
    \* MUTATION flag_after_walk: the flag goes up only after the walk.
    if Mutation # "flag_after_walk" then
        flag := TRUE;
    end if;
C_walk_start:
    \* ... then the walk begins: what is in the registry now is yielded.
    walkStart := registry;
C_walk_end:
    \* The walk ends: every entry present at its start, and any inserted since at the
    \* model's choice, is cancelled.
    \* MUTATION walk_misses_present: one entry present for the whole walk is missed.
    with late \in SUBSET (registry \ walkStart),
         missed \in (IF Mutation = "walk_misses_present" /\ walkStart # {}
                     THEN walkStart ELSE {"none"}) do
        cancelled := [m \in Makers |-> cancelled[m]
                                        \/ (m \in (walkStart \cup late) /\ m # missed)];
    end with;
C_late_flag:
    if Mutation = "flag_after_walk" then
        flag := TRUE;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
