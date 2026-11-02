------------------------------ MODULE MC_TakenFull ------------------------------
(***************************************************************************)
(* The model the configurations check: an owner's write that found the     *)
(* socket full, with the shard maybe taken before the rest of its poll,    *)
(* with its constants set by each configuration (`.cfg`), which says in    *)
(* its first lines what it is about.                                       *)
(***************************************************************************)
EXTENDS TakenFull                                                            \* the model, as it is
=============================================================================
