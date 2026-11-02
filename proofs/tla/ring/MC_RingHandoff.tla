----------------------------- MODULE MC_RingHandoff -----------------------------
(***************************************************************************)
(* The model the handoff configurations check: a ring handed to a new     *)
(* runner while a request its old runner submitted waits in the kernel,   *)
(* with the constants set by each configuration (`.cfg`), which says in   *)
(* its first lines what it is about.                                       *)
(***************************************************************************)
EXTENDS RingHandoff
=============================================================================
