---------------------------- MODULE BroadcastWait ----------------------------
(***************************************************************************)
(* The broadcast channel's waits (src/sync/broadcast.rs: `Receiver::recv`, *)
(* `Receiver::try_recv_if`, `Sender::send`, `Drop for Sender`,             *)
(* `Sender::closed`, `Drop for Receiver`, `Sender::subscribe` and          *)
(* `Shared::subscribe`), at the grain of each atomic access.               *)
(*                                                                          *)
(* In plain words: a receive makes its `Notified` first, then looks: the   *)
(* value of its next position, if published, is taken; else, with the      *)
(* closed flag seen and then the tail at or below its next position, the   *)
(* channel is done; else it waits for the next `notify_waiters` after its  *)
(* `Notified` was made, and looks again. A send counts the receivers,      *)
(* claims a position from the tail, publishes its value there, and calls   *)
(* `notify_waiters`. The last sender to go sets the closed flag and calls  *)
(* it too. `Sender::closed` loops while receivers are left: it makes its   *)
(* `Notified` on the senders' side, reads the count again, and waits; the *)
(* last receiver to go wakes it. A subscribe reads the tail as the new     *)
(* receiver's first position, then counts it in.                           *)
(*                                                                          *)
(* Who acts: the senders, the receivers, one of them made by a subscribe,  *)
(* and a sender waiting in `closed`. What must never happen: a receiver    *)
(* asleep for good with a value or the end to see; the end answered with a *)
(* value it was owed not taken; `closed` asleep after the last receiver    *)
(* went.                                                                   *)
(*                                                                          *)
(* What is abstract: the ring is endless (the lag is Broadcast.tla's), a   *)
(* value is its position; `Notify` is one counter per side, and a          *)
(* `Notified` made when it stood at `g` completes once it passes `g`,      *)
(* which is what `Notify` promises (the notify family checks it).          *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Senders,     \* the senders, numbered 1, 2, ..., each sending once then going
    RxRecvs,     \* receives the first receiver makes (it is dropped after them)
    Subscribe,   \* sender 1 subscribes a second receiver before it sends
    SubRecvs,    \* receives the subscribed receiver makes
    WithClosed,  \* one more sender waits in `closed` until every receiver went
    Mutation     \* "none", or the name of the one rule this run takes away

RxT == 10
SubT == 11
ClosedT == 12
Positions == 0..(Cardinality(Senders) - 1)

(* --algorithm BroadcastWait
variables
    tail = 0,                 \* the next position a send claims
    published = {},           \* positions whose value is in its slot
    closed = FALSE,           \* the closed flag: every sender went
    senders = Cardinality(Senders) + (IF WithClosed THEN 1 ELSE 0),
    receivers = 1,            \* receivers alive
    ngen = 0,                 \* `notify_waiters` calls on the receivers' Notify
    tgen = 0,                 \* `notify_waiters` calls on the senders' Notify
    subscribed = FALSE,       \* the subscribed receiver exists
    subStart = 0,             \* its first position
    next = [q \in {RxT, SubT} |-> 0],     \* each receiver's next position (its cursor)
    res = [q \in {RxT, SubT} |-> "none"], \* what each receiver's last receive answered
    taken = [q \in {RxT, SubT} |-> {}],   \* (record) the positions each receiver took
    endBad = FALSE;           \* (record) an end answered with a published value not taken

define
    \* The end is answered only with every value from the receiver's start taken.
    EndAfterAll == ~endBad
    \* Every receiver's program ends, and so does `closed`.
    AllEnd == <>(\A p \in {RxT} \cup (IF Subscribe THEN {SubT} ELSE {})
                          \cup (IF WithClosed THEN {ClosedT} ELSE {}) : pc[p] = "Done")
end define;

\* One receive, by receiver `r` from its next position `nx`: the answer goes to `res`.
procedure Recv(r)
variables myGen = 0, nx = 0, sawClosed = FALSE;
begin
V_make:
    \* `notify_rx.notified()`, made before the look.
    \* MUTATION notified_after_look: made after it.
    if Mutation # "notified_after_look" then
        myGen := ngen;
    end if;
    nx := next[r];
V_slot:
    \* The slot of the next position: its value, if published.
    if nx \in published then
        taken[r] := taken[r] \cup {nx};
        next[r] := nx + 1;
        res[r] := "value";
        return;
    end if;
V_closed:
    \* The closed flag, then the tail (MUTATION tail_before_closed: the tail first).
    if Mutation = "tail_before_closed" then
        sawClosed := tail <= nx;
    else
        sawClosed := closed;
    end if;
V_tail:
    if (IF Mutation = "tail_before_closed" THEN sawClosed /\ closed ELSE sawClosed /\ tail <= nx) then
        endBad := endBad \/ \E p \in published : p >= nx /\ p \notin taken[r];
        res[r] := "closed";
        return;
    end if;
V_late:
    if Mutation = "notified_after_look" then
        myGen := ngen;
    end if;
V_wait:
    await ngen > myGen;
    goto V_make;
end procedure;

fair process sender \in Senders
variables pos = 0, rc = 0, last = FALSE;
begin
S_sub:
    \* `subscribe`: the tail as the new receiver's start, then counted in.
    if Subscribe /\ self = 1 then
        subStart := tail;
    else
        goto S_count;
    end if;
S_sub_count:
    receivers := receivers + 1;
    subscribed := TRUE;
S_count:
    \* `send`: no receiver, no send.
    rc := receivers;
    if rc = 0 then
        goto S_drop;
    end if;
S_claim:
    pos := tail;
    tail := tail + 1;
S_publish:
    published := published \cup {pos};
S_notify:
    ngen := ngen + 1;
S_drop:
    \* `Drop for Sender`: the last one sets the flag, then notifies.
    \* MUTATION close_no_notify: the flag goes up and nobody is told.
    last := senders = 1;
    senders := senders - 1;
S_close:
    if last then
        closed := TRUE;
    end if;
S_close_notify:
    if last /\ Mutation # "close_no_notify" then
        ngen := ngen + 1;
    end if;
end process;

fair process rx = RxT
variables k = 0, last = FALSE;
begin
X_recv:
    while k < RxRecvs /\ res[RxT] # "closed" do
        call Recv(RxT);
X_next:
        k := k + 1;
    end while;
X_drop:
    \* `Drop for Receiver`: the last one wakes `closed`.
    \* MUTATION last_rx_no_notify: nobody is told.
    last := receivers = 1;
    receivers := receivers - 1;
X_notify:
    if last /\ Mutation # "last_rx_no_notify" then
        tgen := tgen + 1;
    end if;
end process;

fair process sub \in (IF Subscribe THEN {SubT} ELSE {})
variables k = 0, last = FALSE;
begin
U_wait:
    await subscribed;
    next[SubT] := subStart;
U_recv:
    while k < SubRecvs /\ res[SubT] # "closed" do
        call Recv(SubT);
U_next:
        k := k + 1;
    end while;
U_drop:
    last := receivers = 1;
    receivers := receivers - 1;
U_notify:
    if last /\ Mutation # "last_rx_no_notify" then
        tgen := tgen + 1;
    end if;
end process;

fair process waitClosed \in (IF WithClosed THEN {ClosedT} ELSE {})
variables myT = 0, last = FALSE;
begin
C_loop:
    \* `Sender::closed`: while receivers are left.
    if receivers = 0 then
        goto C_drop;
    end if;
C_make:
    \* The Notified first, then the count again.
    \* MUTATION closed_no_recount: no second read of the count.
    myT := tgen;
C_recount:
    if receivers = 0 /\ Mutation # "closed_no_recount" then
        goto C_drop;
    end if;
C_wait:
    await tgen > myT;
    goto C_loop;
C_drop:
    last := senders = 1;
    senders := senders - 1;
C_close:
    if last then
        closed := TRUE;
        ngen := ngen + 1;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
