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
\* One sender: a send_replace whose clone of the old value panics, then a send it never
\* reaches.
OpsReplacePanic == <<<<"replace_panic", "send">>>>
\* One sender: a send_replace, then a send.
OpsReplace == <<<<"replace", "send">>>>
\* One sender sends, subscribes a receiver, and sends again.
OpsSubscribe == <<<<"send", "subscribe", "send">>>>
\* One sender sends, then subscribes a receiver.
OpsSendSubscribe == <<<<"send", "subscribe">>>>
\* One sender clones itself, then sends; the clone sends once.
OpsClone == <<<<"clone", "send">>>>
\* One sender clones itself and goes; the clone sends once.
OpsCloneOnly == <<<<"clone">>>>
=============================================================================
