----------------------------- MODULE MC_LocalChan -----------------------------
(***************************************************************************)
(* The programs the single-shard channel configurations run, named here as *)
(* a configuration file cannot hold a sequence of sequences.               *)
(***************************************************************************)
EXTENDS LocalChan

\* Two senders: one sends twice, one reserves and drops the permit, then sends.
TwoSenders == <<<<"send", "send">>, <<"reserve_drop", "send">>>>
\* A sender that waits in closed() then sends, and one that sends.
ClosedThenSend == <<<<"closed", "send">>, <<"send">>>>
\* A sender that sends twice, and one that waits in closed().
SendAndClosed == <<<<"send", "send">>, <<"closed">>>>
\* One sender that sends, then keeps its handle until the receiver is done.
SendHold == <<<<"send", "hold">>>>
\* One sender that sends once.
OneSend == <<<<"send">>>>
\* One sender that reserves and drops the permit, then keeps its handle.
ReserveDropHold == <<<<"reserve_drop", "hold">>>>
\* One sender that waits in closed().
OneClosed == <<<<"closed">>>>
\* Two senders that each wait in closed().
TwoClosed == <<<<"closed">>, <<"closed">>>>
\* One sender that sends twice.
SendTwice == <<<<"send", "send">>>>
\* ---- the receivers' and the one-shot sender's programs ----
\* The operations <<"close">>.
P_close == <<"close">>
\* The operations <<"close", "recv">>.
P_close_recv == <<"close", "recv">>
\* The operations <<"close", "recv", "recv">>.
P_close_recv_recv == <<"close", "recv", "recv">>
\* The operations <<"drop">>.
P_drop == <<"drop">>
\* The operations <<"park">>.
P_park == <<"park">>
\* The operations <<"park", "send">>.
P_park_send == <<"park", "send">>
\* The operations <<"recv">>.
P_recv == <<"recv">>
\* The operations <<"recv", "close", "recv", "recv">>.
P_recv_close_recv_recv == <<"recv", "close", "recv", "recv">>
\* The operations <<"recv", "recv">>.
P_recv_recv == <<"recv", "recv">>
\* The operations <<"recv", "recv", "recv", "recv">>.
P_recv_recv_recv_recv == <<"recv", "recv", "recv", "recv">>
\* The operations <<"send">>.
P_send == <<"send">>
\* The operations <<"try", "drop">>.
P_try_drop == <<"try", "drop">>
\* The operations <<"try", "recv">>.
P_try_recv == <<"try", "recv">>
=============================================================================
