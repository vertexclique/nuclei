---------------------------- MODULE MC_LocalOneshot ----------------------------
(***************************************************************************)
(* The model the single-shard one-shot configurations check, with its     *)
(* constants set by each configuration (`.cfg`).                          *)
(***************************************************************************)
EXTENDS LocalOneshot
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
