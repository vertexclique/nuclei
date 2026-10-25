----------------------------- MODULE MC_Semaphore -----------------------------
(***************************************************************************)
(* The model the configurations check: the barging semaphore with its     *)
(* constants set by each configuration (`.cfg`), which says in its first   *)
(* lines what it is about. The tasks' programs are named here, as a        *)
(* configuration file cannot hold records.                                 *)
(***************************************************************************)
EXTENDS Semaphore

L(n) == [op |-> "lock", n |-> n]
C(n) == [op |-> "cancel", n |-> n]
\* A mutex: one task locks three times, another once.
MutexBarge == <<<<L(1), L(1), L(1)>>, <<L(1)>>>>
\* A mutex: two tasks lock twice each.
MutexPair == <<<<L(1), L(1)>>, <<L(1), L(1)>>>>
\* A mutex: one task locks, one gives up its wait, one locks.
MutexCancel == <<<<L(1)>>, <<C(1)>>, <<L(1)>>>>
\* Two permits: a request for both behind requests for one.
Weighted == <<<<L(1), L(1)>>, <<L(2)>>>>
\* A mutex: two tasks lock once each.
MutexTwo == <<<<L(1)>>, <<L(1)>>>>
\* A mutex: one task locks, another gives up its wait.
MutexDrop == <<<<L(1)>>, <<C(1)>>>>
\* A mutex: one task locks four times, another once.
MutexBarge4 == <<<<L(1), L(1), L(1), L(1)>>, <<L(1)>>>>
=============================================================================
