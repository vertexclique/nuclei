------------------------------ MODULE MC_Broadcast ---------------------------
(***************************************************************************)
(* The model the broadcast configurations check: senders and a receiver   *)
(* on a ring, with their constants set by each configuration (`.cfg`),     *)
(* which says in its first lines what it is about.                         *)
(***************************************************************************)
EXTENDS Broadcast

\* Every way to rename the senders among themselves (the weak senders are not renamed: they start
\* without a sender of their own). A configuration that names it under SYMMETRY checks a state
\* and its renamings once: sound here, as every sender runs the same code from the same start,
\* and no property singles one sender out.
Perms == Permutations(Senders)
=============================================================================
