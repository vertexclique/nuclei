----------------------------- MODULE MC_PortStarve ----------------------------
(***************************************************************************)
(* The model the starved-reader configurations check: stream reads on a    *)
(* completion port sharing a pool, with the constants each configuration   *)
(* (`.cfg`) sets, which says in its first line what it is about.           *)
(***************************************************************************)
EXTENDS PortStarve
=============================================================================
