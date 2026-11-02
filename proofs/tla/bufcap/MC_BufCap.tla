------------------------------ MODULE MC_BufCap ------------------------------
(***************************************************************************)
(* The model the BufCap configurations check: a worker and the threads   *)
(* that serve its runtime with no shard, each with its pool of IO buffers, *)
(* with their constants set by each configuration (`.cfg`), which says in *)
(* its first lines what it is about.                                      *)
(***************************************************************************)
EXTENDS BufCap                                                               \* the model, as it is
=============================================================================
