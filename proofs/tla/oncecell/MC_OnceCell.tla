------------------------------ MODULE MC_OnceCell ------------------------------
(***************************************************************************)
(* The model the configurations check: the once-cell with its constants   *)
(* set by each configuration (`.cfg`), which says in its first lines what  *)
(* it is about.                                                            *)
(***************************************************************************)
EXTENDS OnceCell

TwoOk == <<"ok", "ok">>
FailThenOk == <<"fail", "ok">>
DropThenOk == <<"drop", "ok">>
OneOk == <<"ok">>
Crowd == <<"fail", "ok", "drop">>
=============================================================================
