------------------------------ MODULE MC_RingPoll ------------------------------
(***************************************************************************)
(* The model the readiness configurations check: a stream read by the     *)
(* ring's multishot poll and straight from the socket, and the switch to  *)
(* and from the multishot receive, with the constants each configuration *)
(* (`.cfg`) sets.                                                          *)
(***************************************************************************)
EXTENDS RingPoll                                                             \* the model, as it is
=============================================================================
