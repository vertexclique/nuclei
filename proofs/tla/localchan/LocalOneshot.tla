----------------------------- MODULE LocalOneshot -----------------------------
(***************************************************************************)
(* The single-shard one-shot channel (src/sync/local/oneshot.rs:           *)
(* `Sender::send`, `Sender::poll_closed`, `Drop for Sender`,               *)
(* `Receiver::close`, `Receiver::try_recv`, `Future for Receiver` (`poll`),*)
(* `Drop for Receiver`), at the grain of one step of a task.               *)
(*                                                                          *)
(* In plain words: the state is three bits in a plain cell (value sent,    *)
(* receiver closed, sender dropped), the value in a cell and the two       *)
(* wakers in cells. A send fails, its value given back, once the receiver  *)
(* closed; otherwise it puts the value, raises the sent and dropped bits   *)
(* and wakes the receiver. A sender dropped without sending raises its bit *)
(* and wakes the receiver. The receiver's poll registers its waker while   *)
(* no bit is up; with any bit up it completes: the value if there is one,  *)
(* else the error. Closing raises the closed bit and wakes a sender        *)
(* waiting in `closed()`; a value sent before the close stays receivable.  *)
(* The receiver's drop closes and drops a value still in the cell.         *)
(*                                                                          *)
(* The tasks run on one thread and no step runs another step, so each      *)
(* operation is one atomic step and the model interleaves whole steps in   *)
(* every order. What must never happen: a value received twice, or lost    *)
(* while the receiver can still take it; a send answered Ok past the       *)
(* close; a task asleep for good with what it waits for there.             *)
(*                                                                          *)
(* What is abstract: a waker is one flag a wake empties; the cooperative   *)
(* budget's refusal is a poll that wakes itself and changes nothing, so it *)
(* is left out; the value is one token.                                    *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Prog,      \* the sender's operations: "send", "drop", "park" (one closed() wait)
    RxProg,    \* the receiver's operations: "recv", "try", "close", "drop"
    Mutation   \* "none", or the name of the one rule this run takes away

Tx == 1
Rx == 0

(* --algorithm LocalOneshot
variables
    valueSent = FALSE, rxClosed = FALSE, txDropped = FALSE,  \* the state cell's three bits
    inCell = FALSE,           \* the value cell holds the value
    rxReg = FALSE,            \* the receiver's waker is in its cell
    rxWoken = FALSE,          \* the receiver's waker was called
    txReg = FALSE,            \* the sender's waker is in its cell
    txWoken = FALSE,          \* the sender's waker was called
    txAlive = TRUE,           \* the sender holds its half
    rxAlive = TRUE,           \* the receiver holds its half
    okSent = FALSE,           \* (record) the send answered Ok
    pastClose = FALSE,        \* (record) a send answered Ok after the close
    back = FALSE,             \* (record) a refused send got its value back
    got = 0,                  \* (record) values the receiver answered
    rxDroppedValue = FALSE;   \* (record) the receiver's drop dropped the value

define
    \* The value is in exactly one place once sent, or back with a refused send.
    ValueConserved ==
        /\ got <= 1
        /\ okSent => (IF inCell THEN 1 ELSE 0) + got + (IF rxDroppedValue THEN 1 ELSE 0) = 1
    NoSendPastClose == ~pastClose
end define;

macro wakeRx() begin
    if rxReg then rxReg := FALSE; rxWoken := TRUE; end if;
end macro;

macro wakeTx() begin
    if txReg then txReg := FALSE; txWoken := TRUE; end if;
end macro;

fair process sender = Tx
variables i = 1;
begin
S_next:
    if i > Len(Prog) \/ ~txAlive then
        goto Done;
    elsif Prog[i] = "send" then
        \* `send`: refused once closed (MUTATION send_ignores_close: never refused).
        if rxClosed /\ Mutation # "send_ignores_close" then
            txDropped := TRUE;
            back := TRUE;
        else
            pastClose := rxClosed;
            inCell := TRUE;
            valueSent := TRUE;
            txDropped := TRUE;
            okSent := TRUE;
            wakeRx();
        end if;
        txAlive := FALSE;
        goto Done;
    elsif Prog[i] = "drop" then
        \* `Drop for Sender`: the bit up, the receiver woken.
        \* MUTATION drop_no_wake: the receiver is not woken.
        txDropped := TRUE;
        if Mutation # "drop_no_wake" then
            wakeRx();
        end if;
        txReg := FALSE;
        txAlive := FALSE;
        goto Done;
    end if;
S_park:
    \* `poll_closed`: ready once closed, else the waker registered and the task asleep.
    if rxClosed then
        i := i + 1;
        goto S_next;
    else
        txReg := TRUE;
    end if;
S_sleep:
    await txWoken;
    txWoken := FALSE;
    goto S_park;
end process;

fair process receiver = Rx
variables ri = 1;
begin
R_next:
    if ri > Len(RxProg) \/ ~rxAlive then
        goto Done;
    elsif RxProg[ri] = "close" then
        \* `close`: the bit up, a sender in closed() woken.
        \* MUTATION close_no_wake: the sender is not woken.
        rxClosed := TRUE;
        if Mutation # "close_no_wake" then
            wakeTx();
        end if;
        ri := ri + 1;
        goto R_next;
    elsif RxProg[ri] = "drop" then
        \* `Drop for Receiver`: closed, the sender woken, the value dropped.
        rxClosed := TRUE;
        wakeTx();
        rxReg := FALSE;
        rxDroppedValue := inCell;
        inCell := FALSE;
        rxAlive := FALSE;
        goto Done;
    end if;
R_poll:
    \* `poll` / `try_recv`: with any bit up the value or the error; else registered (a
    \* poll) or Empty (a try).
    \* MUTATION poll_ignores_dropped: only the sent and closed bits complete a poll.
    if valueSent \/ rxClosed \/ (txDropped /\ Mutation # "poll_ignores_dropped") then
        if inCell then
            got := got + 1;            \* the value
            inCell := FALSE;
        end if;                        \* else the error: no value was ever put
        rxAlive := FALSE;
        goto Done;
    elsif RxProg[ri] = "try" then
        ri := ri + 1;
        goto R_next;
    else
        rxReg := TRUE;
    end if;
R_sleep:
    await rxWoken;
    rxWoken := FALSE;
    goto R_poll;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
