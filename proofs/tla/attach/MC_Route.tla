------------------------------- MODULE MC_Route -------------------------------
(***************************************************************************)
(* The model the route configurations check: a read from a thread that    *)
(* runs no shard while the source's registration finishes, with its       *)
(* constants set by each configuration (`.cfg`), which says in its first  *)
(* lines what it is about.                                                 *)
(***************************************************************************)
EXTENDS Route                                                                \* the model, as it is
=============================================================================
