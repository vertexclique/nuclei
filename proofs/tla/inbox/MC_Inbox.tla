------------------------------ MODULE MC_Inbox ------------------------------
(***************************************************************************)
(* The model the inbox configurations check: pushers and the shard on one  *)
(* queue, with their constants set by each configuration (`.cfg`), which   *)
(* says in its first lines what it is about.                               *)
(***************************************************************************)
EXTENDS Inbox

\* Every way to rename the pushers among themselves. A configuration that names it under
\* SYMMETRY checks a state and its renamings once: sound here, as every pusher runs the same
\* code on its own task and no property singles one pusher out.
Perms == Permutations(Producers)
=============================================================================
