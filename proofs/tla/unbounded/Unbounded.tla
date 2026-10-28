----------------------------- MODULE Unbounded -----------------------------
(***************************************************************************)
(* The unbounded channel's admission (src/sync/mpsc/unbounded.rs) against  *)
(* the receiver's close and its wait for the end, at the grain of each     *)
(* atomic access.                                                           *)
(*                                                                          *)
(* In plain words: a send counts itself in with one fetch-add on the word   *)
(* that holds the values in flight and the closed bit. If the bit was up,  *)
(* it takes itself back out with one fetch-sub, and then wakes the         *)
(* receiver if the closed channel is idle now: the receiver may have seen  *)
(* the count up for that instant. Otherwise it pushes its value and wakes  *)
(* the receiver. The receiver closes, then takes values, giving each one   *)
(* back to the count, and ends once the channel is closed and idle; with   *)
(* nothing to do it registers its waker and looks once more before it      *)
(* sleeps.                                                                  *)
(*                                                                          *)
(* What is abstract: the queue is a count of values pushed and not taken;  *)
(* the waker cell is one flag a wake empties (its handshake is checked by  *)
(* the loom models); a sender keeps the channel open, so only the close    *)
(* and the count decide the end.                                           *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Senders,   \* the sending threads, one send each
    Mutation   \* "none", or the name of the one rule this run takes away

ReceiverT == "receiver"

(* --algorithm Unbounded
variables
    count = 0,            \* values in flight: admitted and not given back
    closed = FALSE,
    queued = 0,           \* values pushed and not taken
    registered = FALSE,   \* the receiver's waker is in the cell
    woken = FALSE,        \* the receiver's waker was called
    received = 0,
    admitted = 0,
    ended = FALSE;

define
    Finished == closed /\ count = 0
end define;

macro wake() begin
    if registered then
        registered := FALSE;
        woken := TRUE;
    end if;
end macro;

fair process sender \in Senders
variables wasClosed = FALSE;
begin
S_add:
    \* One fetch-add; what it read says whether the bit was up.
    wasClosed := closed;
    count := count + 1;
    if wasClosed then
        goto S_undo;
    else
        admitted := admitted + 1;
    end if;
S_push:
    queued := queued + 1;
S_push_wake:
    wake();
    goto S_done;
S_undo:
    \* One fetch-sub takes the refused admission back.
    count := count - 1;
S_undo_wake:
    \* MUTATION no_undo_wake: the refusal does not wake the receiver.
    if Finished /\ Mutation # "no_undo_wake" then
        wake();
    end if;
S_done:
    skip;
end process;

fair process receiver = ReceiverT
begin
R_close:
    closed := TRUE;
R_look:
    if queued > 0 then
        queued := queued - 1;
        received := received + 1;
        goto R_release;
    elsif Finished then
        goto R_end;
    end if;
R_register:
    registered := TRUE;
R_look_again:
    if queued > 0 \/ Finished then
        goto R_look;
    end if;
R_sleep:
    await woken;
    woken := FALSE;
    goto R_look;
R_release:
    count := count - 1;
    goto R_look;
R_end:
    ended := TRUE;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The receiver ends only with every admitted value received.
EndAfterAll == ended => received = admitted

\* The receiver ends.
Ends == <>ended
=============================================================================
