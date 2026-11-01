---------------------------- MODULE MC_PoolIndex ----------------------------
(***************************************************************************)
(* The model the index configurations check: the blocking pool with its    *)
(* bitmap of idle slots, with its constants set by each configuration      *)
(* (`.cfg`), which says in its first lines what it is about.               *)
(***************************************************************************)
EXTENDS PoolIndex                                                            \* the model, as it is
=============================================================================
