------------------------------ MODULE MC_Radix ------------------------------
(***************************************************************************)
(* The model the radix queue configurations check: producers, the         *)
(* receiver and kovan on one queue, with their constants set by each       *)
(* configuration (`.cfg`), which says in its first lines what it is about. *)
(***************************************************************************)
EXTENDS Radix

\* Every way to rename the producers among themselves. A configuration that names it under
\* SYMMETRY checks a state and its renamings once: sound here, as every producer runs the same
\* push from the same start and no property singles one producer out.
Perms == Permutations(Producers)
=============================================================================
