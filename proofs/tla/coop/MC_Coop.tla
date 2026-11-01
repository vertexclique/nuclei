------------------------------- MODULE MC_Coop -------------------------------
(***************************************************************************)
(* The model the configurations check: the cooperative budget with its     *)
(* constants set by each configuration (`.cfg`), which says in its first   *)
(* lines what it is about.                                                 *)
(***************************************************************************)
EXTENDS Coop

\* Three receives on a channel that always has a value.
Recvs == <<"recv", "recv", "recv">>
\* Three acquires of a lock that is free whenever the hot task asks.
Acqs == <<"acq", "acq", "acq">>
\* Both kinds in one poll.
Mixed == <<"recv", "acq", "recv">>
=============================================================================
