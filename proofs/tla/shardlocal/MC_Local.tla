------------------------------- MODULE MC_Local -------------------------------
(***************************************************************************)
(* The model the configurations check: the single-shard semaphore with the *)
(* constants each configuration (`.cfg`) sets and says in its first lines. *)
(***************************************************************************)
EXTENDS Local

\* Two tasks that need both permits, and one that needs one.
NeedsBig == [t \in {"a", "b", "c"} |-> IF t = "c" THEN 1 ELSE 2]
\* One task that needs both permits ("b"), between two that need one.
NeedsMixed == [t \in {"a", "b", "c"} |-> IF t = "b" THEN 2 ELSE 1]
=============================================================================
