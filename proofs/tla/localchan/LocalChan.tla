------------------------------- MODULE LocalChan -------------------------------
(***************************************************************************)
(* The single-shard bounded channel (src/sync/local/mpsc/chan.rs:          *)
(* `Chan::push`, `Chan::poll_recv`, `Chan::try_recv`, `Chan::close_rx`,    *)
(* `Chan::drop_rx`, `Chan::drop_sender`, `Chan::give_back`,                *)
(* `Chan::wake_if_finished`, `ClosedWait::poll`; src/sync/local/mpsc/      *)
(* bounded.rs: `Sender::send`, `Sender::reserve`, `Permit::send`, `Drop   *)
(* for Permit`, `Bounded`'s limit), at the grain of one step of a task.    *)
(*                                                                          *)
(* In plain words: the tasks of one shard share a channel of Cap slots. A  *)
(* send reserves a slot (it waits in line while none is free, and fails    *)
(* once the receiver closed), then pushes its value and wakes the          *)
(* receiver. A reservation can also be dropped unused: its slot goes back, *)
(* and the receiver is woken if that left a closed channel idle. Each      *)
(* sender drops its handle at the end; the last one wakes the receiver.    *)
(* The receiver takes a value (giving its slot back), or answers the end   *)
(* once the channel is closed (by it, or by every sender going), nothing  *)
(* is queued and no slot is reserved, or registers its waker and sleeps.   *)
(* A `closed()` wait of a sender sleeps until the receiver closes; the     *)
(* close wakes every such wait.                                            *)
(*                                                                          *)
(* The tasks run on one thread and every channel step runs no user code   *)
(* between its look and its write, so a step is one atomic step here: the *)
(* model interleaves whole steps in every order, save where a step runs a  *)
(* waker, which is user code:                                              *)
(*  - a registration clones the poll's waker after it looked. With         *)
(*    Reentrant the clone is a step apart (the receiver's old waker out of *)
(*    its cell meanwhile), so the other tasks may send, drop or close      *)
(*    during it, as a clone that touches the channel does; the poll then   *)
(*    looks again before it sleeps. A `closed()` wait clones, then looks,  *)
(*    then joins the list.                                                 *)
(*  - the close wakes the `closed()` waits one at a time; a waker in       *)
(*    WakePanics panics, and a guard wakes the rest before the panic goes  *)
(*    on to the closer's caller.                                           *)
(*  - with RecvPanics the first value received gives its slot back to a   *)
(*    waiting send whose waker panics: the value in hand goes back to the  *)
(*    front of the queue and its slot is taken back as a debt (the count   *)
(*    may go below zero), and the receiver's caller receives again.        *)
(* What must never happen: the receiver answers the end while a value it   *)
(* was owed is not received; a value lost; a slot lost or made up; a task  *)
(* asleep for good with what it waits for there.                           *)
(*                                                                          *)
(* What is abstract: the slots' semaphore is a count and a wait for a free *)
(* slot (its line and its fairness are the localsemaphore family's); a     *)
(* waker is one flag a wake empties; values are counted, not named.        *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Cap,       \* the channel's slots
    Progs,     \* per sender, its operations: "send", "reserve_drop", "closed", "hold"
    RxProg,    \* the receiver's operations: "recv", "try", "close", "drop"
    Reentrant, \* a registration's clone of its waker is a step of its own (user code)
    WakePanics,\* the senders whose closed() waker panics when the close wakes it
    RecvPanics,\* the first value received gives its slot to a send whose waker panics
    Mutation   \* "none", or the name of the one rule this run takes away

Senders == 1..Len(Progs)
ReceiverT == 0

(* --algorithm LocalChan
variables
    free = Cap,               \* slots free in the semaphore
    queued = 0,               \* values in the queue
    reserved = 0,             \* slots reserved by a permit and not used yet
    senders = Len(Progs),     \* sender handles alive
    rxClosed = FALSE,         \* the receiver closed (or was dropped); the semaphore is closed
    rxRegistered = FALSE,     \* the receiver's waker is in its cell
    rxWoken = FALSE,          \* the receiver's waker was called
    closedWaiting = {},       \* `closed()` waits in the list, not woken yet
    closedWoken = {},         \* `closed()` waits woken
    sent = 0,                 \* (record) values pushed
    received = 0,             \* (record) values the receiver took
    rxDropped = 0,            \* (record) values the receiver's drop threw away
    ended = FALSE,            \* (record) the receiver answered the end
    closeStopped = FALSE,     \* the close's loop gave up at a panicking waker
    recvPanicked = FALSE,     \* the one panicking receive happened
    lost = 0;                 \* (record) values taken and dropped

define
    \* `finished`: closed by either side, nothing queued, no slot reserved.
    Finished == (rxClosed \/ senders = 0) /\ queued = 0 /\ free = Cap
    \* Every slot is free, holds a queued value, or is reserved.
    SlotsConserved == free + queued + reserved = Cap
    \* The end is answered only with every value pushed received.
    EndAfterAll == ended => received = sent
    \* No value is ever taken and dropped.
    NoValueLost == lost = 0
end define;

\* `wake_rx`: the receiver's waker leaves its cell and is called.
macro wakeRx() begin
    if rxRegistered then
        rxRegistered := FALSE;
        rxWoken := TRUE;
    end if;
end macro;

fair process sender \in Senders
variables i = 1;
begin
S_next:
    if i > Len(Progs[self]) then
        goto S_drop;
    elsif Progs[self][i] = "closed" then
        goto S_closed;
    elsif Progs[self][i] = "hold" then
        goto S_hold;
    end if;
S_reserve:
    \* `reserve`: a free slot, or the close; it waits in line while neither is there.
    await free > 0 \/ rxClosed;
    if rxClosed then
        goto S_op_done;            \* SendError: the receiver closed
    else
        free := free - 1;
        reserved := reserved + 1;
    end if;
S_use:
    \* The permit's send pushes and wakes the receiver; a dropped permit gives its slot
    \* back and wakes the receiver if that left a closed channel idle.
    \* MUTATION push_no_wake: the push wakes nobody.
    \* MUTATION give_back_no_wake: the given-back slot wakes nobody.
    reserved := reserved - 1;
    if Progs[self][i] = "send" then
        queued := queued + 1;
        sent := sent + 1;
        if Mutation # "push_no_wake" then
            wakeRx();
        end if;
    else
        free := free + 1;
        if Mutation # "give_back_no_wake" /\ (rxClosed \/ senders = 0) /\ queued = 0
           /\ free = Cap then
            wakeRx();
        end if;
    end if;
    goto S_op_done;
S_closed:
    \* `ClosedWait::poll`: ready once the receiver closed, else in the list. With Reentrant the
    \* clone of the waker it joins with runs first, as a step of its own.
    if rxClosed then
        goto S_op_done;
    elsif Reentrant then
        goto S_closed_join;
    else
        closedWaiting := closedWaiting \cup {self};
        goto S_closed_sleep;
    end if;
S_closed_join:
    \* After the clone it looks again, then joins. MUTATION closed_no_relook: it joins without
    \* looking, after a close the clone ran.
    if rxClosed /\ Mutation # "closed_no_relook" then
        goto S_op_done;
    else
        closedWaiting := closedWaiting \cup {self};
    end if;
S_closed_sleep:
    await self \in closedWoken;
    closedWoken := closedWoken \ {self};
    goto S_op_done;
S_hold:
    \* The sender keeps its handle until the receiver's program is over.
    await pc[ReceiverT] = "Done";
S_op_done:
    i := i + 1;
    goto S_next;
S_drop:
    \* `drop_sender`: the last sender wakes the receiver.
    \* MUTATION last_drop_no_wake: it wakes nobody.
    senders := senders - 1;
    if senders = 0 /\ Mutation # "last_drop_no_wake" then
        wakeRx();
    end if;
end process;

fair process receiver = ReceiverT
variables ri = 1;
begin
R_next:
    if ri > Len(RxProg) \/ ended then
        goto Done;
    elsif RxProg[ri] = "close" then
        goto R_close;
    elsif RxProg[ri] = "drop" then
        goto R_drop;
    end if;
R_poll:
    \* `poll_recv` (or `try_recv`): a value, the end, or the waker registered.
    \* MUTATION finish_ignores_reserved: the end does not wait for reserved slots.
    if queued > 0 then
        queued := queued - 1;
        free := free + 1;              \* the slot given back: a waiting send may take it
        if RecvPanics /\ ~recvPanicked then
            recvPanicked := TRUE;      \* that send's waker panics, the value in hand
            goto R_unwind;
        else
            received := received + 1;
            goto R_op_done;
        end if;
    elsif Finished \/ (Mutation = "finish_ignores_reserved" /\ (rxClosed \/ senders = 0)
                       /\ queued = 0) then
        ended := TRUE;
        goto Done;
    elsif RxProg[ri] = "try" then
        goto R_op_done;            \* Empty
    elsif Reentrant then
        rxRegistered := FALSE;     \* the cell empty while the clone runs
        goto R_set;
    else
        rxRegistered := TRUE;
    end if;
R_sleep:
    await rxWoken;
    rxWoken := FALSE;
    goto R_poll;
R_set:
    \* The clone is stored; a clone ran, so the poll looks again before it sleeps.
    \* MUTATION no_relook: it answers Pending without looking.
    rxRegistered := TRUE;
    if (queued > 0 \/ Finished) /\ Mutation # "no_relook" then
        goto R_poll;               \* the look again finds a value or the end
    else
        goto R_sleep;
    end if;
R_unwind:
    \* The receive's guard puts the value back at the front of the queue and takes its slot back
    \* as a debt, then the panic reaches the receiver's caller, which receives again.
    \* MUTATION pop_lost: no guard; the value is dropped.
    if Mutation = "pop_lost" then
        lost := lost + 1;
    else
        queued := queued + 1;
        free := free - 1;
    end if;
    goto R_poll;
R_close:
    \* `close_rx`: the semaphore closed, then every `closed()` wait woken.
    \* MUTATION close_no_wake: the waits are left asleep.
    if ~rxClosed then
        rxClosed := TRUE;
        if Mutation = "close_no_wake" then
            goto R_op_done;
        end if;
    else
        goto R_op_done;
    end if;
R_close_wake:
    \* The close's loop: one wait at a time. A waker in WakePanics panics; the guard wakes the
    \* rest, then the panic reaches the closer's caller, which goes on.
    \* MUTATION close_stops_at_panic: the loop ends at the panic, the rest asleep for good.
    while closedWaiting # {} /\ ~closeStopped do
        with w \in closedWaiting do
            closedWaiting := closedWaiting \ {w};
            if w \in WakePanics then
                closeStopped := Mutation = "close_stops_at_panic";
            else
                closedWoken := closedWoken \cup {w};
            end if;
        end with;
    end while;
    goto R_op_done;
R_drop:
    \* `drop_rx`: close (as `close_rx`), let go of the waker, drop every queued value giving
    \* its slot back.
    if ~rxClosed then
        rxClosed := TRUE;
        if Mutation # "close_no_wake" then
            closedWoken := closedWoken \cup closedWaiting;
            closedWaiting := {};
        end if;
    end if;
    rxRegistered := FALSE;
    rxDropped := rxDropped + queued;
    free := free + queued;
    queued := 0;
    goto Done;
R_op_done:
    ri := ri + 1;
    goto R_next;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

\* Every task's program finishes: nobody sleeps for good with what it waits for there.
AllEnd == <>(\A p \in Senders \cup {ReceiverT} : pc[p] = "Done")
\* Every task whose waker works finishes: a panicking waker strands nobody but its own task.
OthersEnd == <>(\A p \in (Senders \ WakePanics) \cup {ReceiverT} : pc[p] = "Done")
=============================================================================
