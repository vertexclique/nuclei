------------------------------ MODULE MC_RingInc ------------------------------
(***************************************************************************)
(* The model the incremental configurations check: multishot receives on a *)
(* receive group filled in pieces, with its constants set by each          *)
(* configuration (`.cfg`), which says in its first lines what it is about. *)
(***************************************************************************)
EXTENDS RingInc                                                              \* the model, as it is
=============================================================================
