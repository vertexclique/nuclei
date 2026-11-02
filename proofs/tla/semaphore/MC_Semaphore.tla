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
\* A mutex: one task locks twice, another once.
PairOneTwo == <<<<L(1), L(1)>>, <<L(1)>>>>
\* A mutex: one task locks twice, another gives up its wait.
LockTwiceDrop == <<<<L(1), L(1)>>, <<C(1)>>>>
\* Two single locks against a weighted wait for both permits that is dropped.
WeightedDrop == <<<<L(1), L(1)>>, <<C(2)>>>>
\* `add_permits(1)`, `forget_permits(1)`, and a lock of n given back in two releases.
A == [op |-> "add", n |-> 0]
F == [op |-> "forget", n |-> 0]
S(n) == [op |-> "split", n |-> n]
\* A lock on a semaphore of no permits, against a task that adds one.
AddWait == <<<<L(1)>>, <<A>>>>
\* A lock of both permits given back in two releases, against a lock and a forget.
SplitForget == <<<<S(2)>>, <<L(1), F>>>>
=============================================================================
