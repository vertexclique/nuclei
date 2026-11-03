-------------------------------- MODULE MC_Stall --------------------------------
(***************************************************************************)
(* The model the configurations check: the stalled queue with its          *)
(* constants set by each configuration (`.cfg`), which says in its first   *)
(* lines what it is about.                                                 *)
(***************************************************************************)
EXTENDS Stall

\* Waiter 1's frame runs waiter 2, whose frame sits on top of it.
OneRunsTwo == {<<1, 2>>}
\* Waiter 1's frame runs waiter 2, whose frame runs waiter 3.
Chain == {<<1, 2>>, <<2, 3>>}
=============================================================================
