---------------------------- MODULE MC_Unbounded ----------------------------
(***************************************************************************)
(* The model the unbounded channel configurations check: senders and the  *)
(* receiver on one channel, with their constants set by each configuration *)
(* (`.cfg`), which says in its first lines what it is about. The receiver *)
(* programs are named here.                                                *)
(***************************************************************************)
EXTENDS Unbounded

\* Close, then receive until the end.
RxCloseRecv == <<"close", "recv", "recv", "recv">>
\* Receive one by one, one batch, until the end.
RxMixed == <<"recv", "many", "recv", "recv">>
\* Receive until the end.
RxRecvs == <<"recv", "recv", "recv">>
\* One receive.
RxOne == <<"recv">>
\* Two receives.
RxTwo == <<"recv", "recv">>
\* Close, then one receive.
RxCloseOne == <<"close", "recv">>
\* Try, then drop.
RxTryDrop == <<"try", "drop">>
\* Tries around a close.
RxTries == <<"try", "try", "close", "try", "try">>
=============================================================================
