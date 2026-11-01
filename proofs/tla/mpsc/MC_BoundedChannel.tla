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
\* (P_many, P_cancel_many, P_cancel_one and P_large_small, four operations each, are run
\* only by mutations and witnesses, which stop at the first state that breaks their check.)

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

\* ---- smaller programs for the runs that must pass: sender 1 makes the one request the
\*      run is about, sender 2 only sends, once or twice ----

\* A request for both slots, putting one value in, racing two single sends.
P_many_vs2 == [s \in {1, 2} |-> IF s = 1 THEN <<Res(2, 1)>> ELSE <<Send, Send>>]
\* A request for both slots that puts nothing in, racing two single sends.
P_unused_vs2 == [s \in {1, 2} |-> IF s = 1 THEN <<Res(2, 0)>> ELSE <<Send, Send>>]
\* A request for both slots polled once and dropped, racing two single sends.
P_cancel_many_vs2 == [s \in {1, 2} |-> IF s = 1 THEN <<Cancel(2)>> ELSE <<Send, Send>>]
\* A send polled once and dropped, racing two single sends.
P_cancel_one_vs2 == [s \in {1, 2} |-> IF s = 1 THEN <<Cancel(1)>> ELSE <<Send, Send>>]
\* A request for both slots, putting one value in, racing one single send.
P_many_vs1 == [s \in {1, 2} |-> IF s = 1 THEN <<Res(2, 1)>> ELSE <<Send>>]
\* A request for both slots polled once and dropped, then a send, racing one single send.
P_cancel_send_vs1 == [s \in {1, 2} |-> IF s = 1 THEN <<Cancel(2), Send>> ELSE <<Send>>]

\* ---- one sender alone, for the liveness checks, which cost far more per state ----

\* Three sends on one slot: each send after the first waits for the slot of the value before.
P_solo_sends == [s \in {1} |-> <<Send, Send, Send>>]
\* A send, a send polled once and dropped, and a send that needs the slot the drop gives back.
P_solo_cancel == [s \in {1} |-> <<Send, Cancel(1), Send>>]
\* A send, then a request for both slots that waits for the slot the value holds.
P_solo_many == [s \in {1} |-> <<Send, Res(2, 1)>>]

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
