--------------------------- MODULE MC_Oneshot ---------------------------
(***************************************************************************)
(* The programs the configurations run. A configuration picks one program  *)
(* for the sender and one for the receiver, and names them here with      *)
(* `Prog <- P_...` and `RxProg <- R_...`.                                  *)
(*                                                                        *)
(* A sender program is a sequence of "send" (send the value; the sender   *)
(* is spent), "park" (one poll of closed(), which may sleep), and "drop"  *)
(* (drop the handle without sending). Send and drop end the program: the  *)
(* Rust methods consume the sender. A receiver program is a sequence of   *)
(* "recv" (poll to completion), "try" (one try_recv; Empty keeps going),  *)
(* "close" and "drop"; a completing operation ends the program, because a *)
(* completed receiver answers nothing more (the code panics), so recv and *)
(* drop only ever come last.                                              *)
(***************************************************************************)
EXTENDS Oneshot

\* ---- sender programs ----

\* Send the value, nothing before or after.
P_send == <<"send">>
\* Drop the sender without sending: the receiver fails.
P_drop == <<"drop">>
\* Wait in closed() first (it may sleep), then send.
P_park_send == <<"park", "send">>
\* Wait in closed() first (it may sleep), then drop the handle.
P_park_drop == <<"park", "drop">>

\* ---- receiver programs ----

\* Await the value or the error.
R_recv == <<"recv">>
\* One try_recv, then await whatever comes.
R_try == <<"try", "recv">>
\* One try_recv, then drop the handle.
R_try_drop == <<"try", "drop">>
\* Close the channel, then keep the handle.
R_close_keep == <<"close">>
\* Close the channel, then await: a value sent before the close still arrives.
R_close_recv == <<"close", "recv">>
\* Drop the receiver.
R_drop == <<"drop">>
=============================================================================
