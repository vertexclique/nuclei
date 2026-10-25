------------------------------- MODULE MC_Event -------------------------------
(***************************************************************************)
(* The model the configurations check: the event and the latch with their *)
(* constants set by each configuration (`.cfg`), which says in its first   *)
(* lines what it is about.                                                 *)
(***************************************************************************)
EXTENDS Event

TwoAwait == <<"await", "await">>
DropAwait == <<"drop", "await">>
=============================================================================
