------------------------------- MODULE MC_Notify -------------------------------
(***************************************************************************)
(* The model the configurations check: Notify with its constants set by   *)
(* each configuration (`.cfg`), which says in its first lines what it is   *)
(* about.                                                                  *)
(***************************************************************************)
EXTENDS Notify

\* Two waiters that await.
TwoAwait == <<"await", "await">>
\* One waiter dropped, one that awaits.
DropAwait == <<"drop", "await">>
\* One waiter that awaits.
OneAwait == <<"await">>
\* Calls.
Broadcast == <<"waiters">>
OneThenBroadcast == <<"one", "waiters">>
TwoOnes == <<"one", "one">>
OneOne == <<"one">>
LastThenOne == <<"last", "one">>
=============================================================================
