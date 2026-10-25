------------------------------- MODULE MC_Watch -----------------------------
(***************************************************************************)
(* The model the configurations check: the watch channel with its         *)
(* constants set by each configuration (`.cfg`), which says in its first   *)
(* lines what it is about. The senders' operations are named here, as a    *)
(* configuration file cannot hold a sequence of sequences.                 *)
(***************************************************************************)
EXTENDS Watch

\* Two senders: one sends twice, one once.
OpsSends == <<<<"send", "send">>, <<"send">>>>
\* Two senders modify in place: twice and once.
OpsModifies == <<<<"modify", "modify">>, <<"modify">>>>
\* A send and a modifying send against a modifying send.
OpsMixed == <<<<"send", "modify">>, <<"modify">>>>
\* One sender, one send.
OpsOne == <<<<"send">>>>
\* One sender, two sends.
OpsTwo == <<<<"send", "send">>>>
\* Two senders, one send each.
OpsTwoSenders == <<<<"send">>, <<"send">>>>
\* Two senders, one modify each.
OpsTwoModifiers == <<<<"modify">>, <<"modify">>>>
=============================================================================
