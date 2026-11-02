----------------------------- MODULE Unbounded -----------------------------
(***************************************************************************)
(* The unbounded channel (src/sync/mpsc/unbounded.rs: `Unbounded::admit`,  *)
(* `UnboundedSender::send`, `WeakUnboundedSender::upgrade`, `Drop for      *)
(* UnboundedSender`; src/sync/mpsc/chan.rs: `Chan::send`, `Chan::pop`,     *)
(* `Chan::next`, `Chan::poll_recv`, `Chan::take_many`,                     *)
(* `Chan::poll_recv_many`, `Chan::try_recv`, `Chan::close_rx`,             *)
(* `Chan::drop_rx`, `Chan::drop_sender`, `Chan::try_add_sender`,           *)
(* `Chan::wake_if_finished`), at the grain of each atomic access. The      *)
(* `Chan` steps are the bounded channel's too: only the limit differs.     *)
(*                                                                          *)
(* In plain words: a send counts itself in with one fetch-add on the word   *)
(* that holds the values in flight and the closed bit. If the bit was up,  *)
(* it takes itself back out with one fetch-sub, and then wakes the         *)
(* receiver if the closed channel is idle now: the receiver may have seen  *)
(* the count up for that instant. Otherwise it pushes its value (the queue *)
(* claims a cell, then the value is written and marked ready), then looks  *)
(* at the receiver's "I wait" flag and, if it is up, lowers it and wakes   *)
(* the receiver. A sender's drop takes it out of the sender count; the     *)
(* last one wakes the receiver. A clone counts itself in while its        *)
(* original still holds a count. A weak sender upgrades with a             *)
(* compare-and-swap that refuses once the count is zero.                   *)
(*                                                                          *)
(* The receiver looks at the queue (`pop`): a ready value is taken; else   *)
(* it reads the sender count, and only a count of zero sends it to look at *)
(* the queue once more before it answers the end. A taken value is given  *)
(* back to the count. With nothing to take it registers its waker, raises  *)
(* its flag, and looks again; still nothing, it answers the end if the     *)
(* channel is closed and idle, and otherwise sleeps until woken. A batch   *)
(* receive takes up to two values and gives them back in one step. A try  *)
(* receive looks once. The close sets the closed bit; the drop closes,     *)
(* lets go of its waker, and drops every ready value, giving each back.    *)
(*                                                                          *)
(* Who acts: the senders (each sends once, then drops), a clone one of     *)
(* them makes, a weak sender that upgrades, and the receiver. What must    *)
(* never happen: the receiver answers the end while a value it was owed is *)
(* not received; a receiver that could be woken sleeps for good.           *)
(*                                                                          *)
(* What is abstract: the queue is a count of ready values and a count of   *)
(* cells claimed and not yet written, and a pop takes any ready value (the *)
(* radix queue's take-ahead and its order are the radix family's part);    *)
(* the waker cell is one flag a wake empties (its handshake is checked by  *)
(* the loom models); the closed event that `Sender::closed` waits on is    *)
(* the event family's part; a try_add_sender's retry loop is one step.     *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Senders,   \* the strong senders at the start, each one send then a drop
    KeepOpen,  \* one more strong sender, never dropped: only the close ends the channel
    WithClone, \* the first sender clones itself before its send; the clone sends once
    WithWeak,  \* a weak sender upgrades, and if it may, sends once
    RxProg,    \* the receiver's operations: "recv", "many", "try", "close", "drop"
    Mutation   \* "none", or the name of the one rule this run takes away

ReceiverT == "receiver"     \* the receiver's thread
CloneT == "clone"           \* the clone's thread
WeakT == "weak"             \* the weak sender's thread
\* The sender that clones itself.
Cloner == CHOOSE s \in Senders : TRUE

(* --algorithm Unbounded
variables
    count = 0,            \* values in flight: admitted and not given back
    closed = FALSE,       \* the closed bit of the same word
    ready = 0,            \* values written and marked ready in the queue, not taken
    writing = 0,          \* cells claimed and not written yet
    txCount = Cardinality(Senders) + (IF KeepOpen THEN 1 ELSE 0), \* strong senders alive
    rxWaiting = FALSE,    \* the receiver's flag: it registered and is about to sleep
    registered = FALSE,   \* the receiver's waker is in the cell
    woken = FALSE,        \* the receiver's waker was called
    cloned = FALSE,       \* the clone exists
    got = "empty",        \* what the receiver's last pop answered
    received = 0,         \* (record) values the receiver took
    rxDropped = 0,        \* (record) values the receiver's drop threw away
    admitted = 0,         \* (record) values admitted
    ended = FALSE,        \* (record) the receiver answered the end of the channel
    rxGone = FALSE;       \* the receiver was dropped

define
    \* `Chan::finished`: closed, and nothing admitted on its way.
    Finished == closed /\ count = 0
end define;

\* `rx_waker.wake()`: the cell gives the waker up and the receiver's task is told.
macro wake() begin
    if registered then
        registered := FALSE;     \* the waker leaves the cell ...
        woken := TRUE;           \* ... and the receiver's task is told
    end if;
end macro;

\* One `UnboundedSender::send`, by any sender.
procedure Send()
variables wasClosed = FALSE;     \* what the fetch-add read of the closed bit
begin
S_add:
    \* One fetch-add; what it read says whether the bit was up.
    \* MUTATION admit_ignores_close: the bit is not looked at, every value gets in.
    wasClosed := closed /\ Mutation # "admit_ignores_close";
    count := count + 1;
    if wasClosed then
        goto S_undo;               \* refused: take the count back
    else
        admitted := admitted + 1;  \* (record) the value is the receiver's to take
    end if;
S_claim:
    \* The queue's fetch-add claims a cell.
    writing := writing + 1;
    \* MUTATION flag_before_write: the send looks at the flag before it writes.
    if Mutation = "flag_before_write" then
        goto S_look;
    end if;
S_write:
    \* The value written and its cell marked ready (sequentially consistent).
    writing := writing - 1;
    ready := ready + 1;
    if Mutation = "flag_before_write" then
        return;                    \* the mutation looked already
    end if;
S_look:
    \* The sequentially consistent look at the flag; up, it is swapped down and the
    \* receiver woken (`wake_waiting_rx`).
    \* MUTATION push_no_wake: the send never wakes the receiver.
    if rxWaiting /\ Mutation # "push_no_wake" then
        rxWaiting := FALSE;
        wake();
    end if;
    if Mutation = "flag_before_write" then
        goto S_write;              \* the mutation writes only now
    else
        return;
    end if;
S_undo:
    \* One fetch-sub takes the refused admission back.
    count := count - 1;
S_undo_wake:
    \* `wake_if_finished`: the receiver may have seen the count up for that instant.
    \* MUTATION no_undo_wake: the refusal does not wake the receiver.
    if Finished /\ Mutation # "no_undo_wake" then
        wake();
    end if;
    return;
end procedure;

\* `Chan::drop_sender`: the count down by one; the last one wakes the receiver.
procedure DropSender()
begin
D_sub:
    \* One fetch-sub; the one that read one was the last sender.
    \* MUTATION last_drop_no_wake: the last sender goes without waking the receiver.
    txCount := txCount - 1;
    if txCount = 0 /\ Mutation # "last_drop_no_wake" then
        wake();
    end if;
    return;
end procedure;

\* `Chan::pop`: a ready value; else the sender count; at zero, the queue once more.
\* The answer is left in `got`: "value", "closed" or "empty".
procedure Pop()
begin
P_take:
    \* `try_pop`: a ready value is taken.
    if ready > 0 then
        ready := ready - 1;
        got := "value";
        return;
    end if;
P_count:
    \* The sender count: above zero, the queue is only empty for now.
    if txCount # 0 then
        got := "empty";
        return;
    end if;
P_again:
    \* The count read zero only after every sender's pushes: one more look is final.
    \* MUTATION pop_no_second_look: the end is answered without it.
    if ready > 0 /\ Mutation # "pop_no_second_look" then
        ready := ready - 1;
        got := "value";
    else
        got := "closed";
    end if;
    return;
end procedure;

\* A strong sender: the cloner clones first; each sends once, then drops.
fair process sender \in Senders
begin
T_clone:
    \* `clone`: the count up by one while this sender holds its own.
    if WithClone /\ self = Cloner then
        txCount := txCount + 1;
        cloned := TRUE;
    end if;
T_send:
    call Send();
T_drop:
    call DropSender();
end process;

\* The clone: once it exists it sends once, then drops.
fair process clone \in (IF WithClone THEN {CloneT} ELSE {})
begin
C_wait:
    await cloned;
C_send:
    call Send();
C_drop:
    call DropSender();
end process;

\* The weak sender: upgrade, and with a strong sender in hand send once and drop it.
fair process weak \in (IF WithWeak THEN {WeakT} ELSE {})
begin
K_upgrade:
    \* `try_add_sender`: refuses at zero, else the count up by one.
    \* MUTATION upgrade_from_zero: the count goes up from zero as well.
    if txCount = 0 /\ Mutation # "upgrade_from_zero" then
        goto Done;
    else
        txCount := txCount + 1;
    end if;
K_send:
    call Send();
K_drop:
    call DropSender();
end process;

\* The receiver runs RxProg; an answered end, or the drop, ends the program.
fair process receiver = ReceiverT
variables ri = 1,          \* the receiver's operation
          taken = 0;       \* values taken and not given back yet
begin
R_next:
    if ri > Len(RxProg) \/ ended \/ rxGone then
        goto Done;
    elsif RxProg[ri] = "close" then
        goto R_close;
    elsif RxProg[ri] = "drop" then
        goto R_drop;
    end if;
R_look:
    \* `next`, or for "many" the first pop of `take_many`.
    call Pop();
R_first:
    if got = "value" then
        goto R_took;
    elsif got = "closed" then
        goto R_end;
    elsif RxProg[ri] = "try" then
        \* `try_recv`: an empty queue is the end only once the channel is finished.
        if Finished then
            goto R_end;
        else
            goto R_op_done;
        end if;
    end if;
R_register:
    \* The waker into the cell, then the flag up (`rx_says_it_waits`).
    \* MUTATION raise_after_look: the flag goes up only after the look again.
    \* MUTATION rx_no_recheck: no look again after the registration.
    registered := TRUE;
    if Mutation # "raise_after_look" then
        rxWaiting := TRUE;
    end if;
    if Mutation = "rx_no_recheck" then
        goto R_finished;
    end if;
R_look_again:
    call Pop();
R_second:
    \* A value found: `rx_does_not_wait` lowers the flag (the mutation raises it only now).
    rxWaiting := IF got = "value" THEN FALSE
                 ELSE IF Mutation = "raise_after_look" THEN TRUE ELSE rxWaiting;
    if got = "value" then
        goto R_took;
    elsif got = "closed" then
        goto R_end;
    end if;
R_finished:
    \* Nothing even now: the end once closed and idle, else sleep.
    \* MUTATION finish_early: the end once closed, whatever is still on its way.
    if Finished \/ (Mutation = "finish_early" /\ closed) then
        goto R_end;
    end if;
R_sleep:
    \* `Pending`: asleep until the waker is called; then the poll starts over.
    await woken;
    woken := FALSE;
    goto R_look;
R_took:
    \* A value taken; for "many", one more pop before the one give-back.
    received := received + 1;
    taken := 1;
    if RxProg[ri] = "many" then
        call Pop();
    else
        goto R_release;
    end if;
R_more:
    if got = "value" then
        received := received + 1;
        taken := 2;
    end if;
R_release:
    \* The values taken given back to the count in one fetch-sub.
    count := count - taken;
    taken := 0;
    goto R_op_done;
R_close:
    \* `close_rx`: the closed bit goes up.
    closed := TRUE;
    goto R_op_done;
R_drop:
    \* `drop_rx`: close, and let go of the waker.
    closed := TRUE;
    registered := FALSE;
    rxGone := TRUE;
R_drain:
    \* Every ready value dropped and given back; a value still being written stays
    \* queued and is dropped with the channel.
    if ready > 0 then
        ready := ready - 1;
        rxDropped := rxDropped + 1;
        count := count - 1;
        goto R_drain;
    else
        goto Done;
    end if;
R_end:
    ended := TRUE;             \* (record) the end of the channel was answered
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

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The receiver answers the end only with every admitted value received.
EndAfterAll == ended => received = admitted

\* The count is the values admitted and not given back: never below zero.
CountSound == count >= 0

\* The receiver's program finishes: it is never left asleep with a wake owed.
Ends == <>(pc[ReceiverT] = "Done")
=============================================================================
