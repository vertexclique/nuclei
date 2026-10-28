------------------------------- MODULE MC_Barrier -------------------------------
(***************************************************************************)
(* The model the configurations check: the barrier with its constants set *)
(* by each configuration (`.cfg`), which says in its first lines what it   *)
(* is about.                                                               *)
(***************************************************************************)
EXTENDS Barrier

\* Two tasks, two waits each, through a barrier of two: two generations.
TwoTwo == <<2, 2>>
\* Three tasks, one wait each, through a barrier of two.
ThreeOne == <<1, 1, 1>>
=============================================================================
