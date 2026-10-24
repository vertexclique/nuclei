------------------------- MODULE MC_BoundedChannel -------------------------
(***************************************************************************)
(* The programs the configurations run. A configuration picks the channel *)
(* size, the senders, one program for them and one for the receiver, and  *)
(* names them here with `Prog <- P_...` and `RxProg <- R_...`.            *)
(***************************************************************************)
EXTENDS BoundedChannel

\* `reserve_many(n)` (or `send` when n = 1): wait for n slots, put u values in through them,
\* give the rest back.
Res(n, u) == [op |-> "reserve", n |-> n, use |-> u]
\* `send(v)`: one slot, one value.
Send == Res(1, 1)
\* `try_reserve_many(n)`: the same, but never waiting: a full channel refuses it.
Try(n, u) == [op |-> "try", n |-> n, use |-> u]
\* `reserve_many(n)` polled once and then dropped (its slots, if any, given straight back).
Cancel(n) == [op |-> "cancel", n |-> n, use |-> 0]

\* ---- sender programs: [sender |-> its operations, in order] ----

\* Two senders on one slot: three sends, so somebody always waits.
P_sends == [s \in {1, 2} |-> IF s = 1 THEN <<Send, Send>> ELSE <<Send>>]
\* A request for both slots queued between single sends: a share is set aside for it.
P_many == [s \in {1, 2} |-> IF s = 1 THEN <<Send, Res(2, 1)>> ELSE <<Send, Send>>]
\* A request for both slots that gives up after one poll, between two single sends.
P_cancel_many == [s \in {1, 2} |-> IF s = 1 THEN <<Send, Cancel(2)>> ELSE <<Send, Send>>]
\* A single send that gives up after one poll, racing another sender's sends.
P_cancel_one == [s \in {1, 2} |-> IF s = 1 THEN <<Send, Cancel(1)>> ELSE <<Send, Send>>]
\* A `try_` racing sends that wait.
P_try == [s \in {1, 2} |-> IF s = 1 THEN <<Send, Send>> ELSE <<Try(1, 1)>>]
\* One sender: two values through one reservation, for the order of one sender's values.
P_pair == [s \in {1} |-> <<Res(2, 2)>>]
\* A large request behind a small one and in front of another small one.
P_large_small == [s \in {1, 2} |-> IF s = 1 THEN <<Send, Res(2, 0)>> ELSE <<Send, Send>>]

\* ---- smaller programs for the liveness checks, which cost far more per state ----

\* A send polled once and dropped, racing two sends on one slot.
P_live_cancel == [s \in {1, 2} |-> IF s = 1 THEN <<Cancel(1)>> ELSE <<Send, Send>>]
\* A request for both slots, putting one value in, racing two single sends.
P_live_many == [s \in {1, 2} |-> IF s = 1 THEN <<Res(2, 1)>> ELSE <<Send, Send>>]

\* ---- receiver programs ----

\* Receive until the end of the channel.
R_drain == <<"drain">>
\* Receive once, close, then receive until the end.
R_close == <<"recv", "close", "drain">>
\* Close at once, then receive until the end.
R_close_first == <<"close", "drain">>
\* Receive once, then drop the receiver.
R_drop == <<"recv", "drop">>
=============================================================================
