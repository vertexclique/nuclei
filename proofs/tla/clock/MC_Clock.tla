------------------------------ MODULE MC_Clock ------------------------------
(***************************************************************************)
(* The model the configurations check: the paused clock's protocol with   *)
(* its constants set by each configuration (`.cfg`), which says in its     *)
(* first lines what it is about.                                           *)
(***************************************************************************)
EXTENDS Clock

\* Every way to rename the shards among themselves. A configuration that names it under SYMMETRY
\* checks a state and its renamings once: sound only where the shards are alike, that is with no
\* blocking job (the job gives its work to shard 1), as every shard runs the same code and the
\* pusher may give its work to any of them, and no invariant singles one shard out.
Perms == Permutations(Shards)
=============================================================================
