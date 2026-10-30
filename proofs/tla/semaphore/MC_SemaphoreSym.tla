--------------------------- MODULE MC_SemaphoreSym ---------------------------
(***************************************************************************)
(* The model the configurations that swap tasks check: the barging        *)
(* semaphore of MC_Semaphore, its tasks named by model values (t1, t2, t3) *)
(* rather than by number, so that a symmetry set can swap two tasks that   *)
(* run the same program. Each configuration (`.cfg`) says in its first     *)
(* lines what it is about.                                                 *)
(***************************************************************************)
EXTENDS MC_Semaphore

\* Three names for tasks, model values a symmetry set may swap.
CONSTANTS t1, t2, t3

\* MutexCancel, its tasks named: t1 and t3 lock, t2 gives up its wait.
MutexCancelNamed == (t1 :> <<L(1)>>) @@ (t2 :> <<C(1)>>) @@ (t3 :> <<L(1)>>)
\* MutexPair, its tasks named: t1 and t2 lock twice each.
MutexPairNamed == (t1 :> <<L(1), L(1)>>) @@ (t2 :> <<L(1), L(1)>>)

\* Every way to rename the two lockers of MutexCancelNamed between themselves.
\* A configuration that names it under SYMMETRY checks a state and its renaming
\* once: sound here, as both run the same program from the same start, and no
\* property singles one task out (each reads every task, or every node, alike).
LockersSwapped == Permutations({t1, t3})
\* The same for the two tasks of MutexPairNamed.
PairSwapped == Permutations({t1, t2})
=============================================================================
