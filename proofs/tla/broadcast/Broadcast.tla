------------------------------ MODULE Broadcast ------------------------------
(***************************************************************************)
(* The broadcast channel's senders and a receiver (src/sync/broadcast.rs:  *)
(* `Sender::send`, `Receiver::try_recv`, the last sender's drop,           *)
(* `WeakSender::upgrade`), at the grain of each atomic access.             *)
(*                                                                          *)
(* In plain words: the channel is a ring of `Cap` slots, each holding the  *)
(* position of the latest value sent to it. A sender takes the next        *)
(* position from the tail, then publishes it into the position's slot,     *)
(* unless the slot holds a later position already (a send a ring later got *)
(* there first: the value goes, every receiver is past it). The last       *)
(* sender to go closes the channel. A weak sender's upgrade counts one     *)
(* more sender only while one is left (a compare-and-swap on the count);   *)
(* the sender it makes sends and goes as any does. The receiver looks at   *)
(* the slot of its next position: its own position there is a value taken; *)
(* a later one is a lag, and it goes on from the oldest position the ring  *)
(* can hold (the tail, read then, less `Cap`), counting what it skipped;   *)
(* anything else is nothing yet, unless the channel is closed and its next *)
(* position is the tail: then it is done. What it took plus what it heard  *)
(* it lost must be every value sent.                                       *)
(*                                                                          *)
(* What is abstract: a value is its position; a wait for a send is the     *)
(* receiver looking again (the wake is `Notify`'s, checked with the sync   *)
(* primitives).                                                             *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Cap,        \* slots in the ring
    Senders,    \* the senders, numbered 1, 2, ...
    Weak,       \* the weak senders, each trying one upgrade, numbered after them
    PerSender,  \* values each sender sends
    Mutation    \* "none", or the name of the one rule this run takes away

ReceiverT == 0

(* --algorithm Broadcast
variables
    tail = 0,                          \* the next position a send takes
    slots = [i \in 0..(Cap - 1) |-> -1], \* the position each slot holds; -1 none
    live = Cardinality(Senders),       \* senders not gone
    closed = FALSE,
    next = 0,                          \* the receiver's next position
    taken = 0,
    lost = 0,
    done = FALSE,                      \* the receiver saw the close
    upgraded = 0,                      \* weak senders whose upgrade made a sender
    lagged = FALSE,                    \* (record) the receiver heard of a lag
    superseded = FALSE,                \* (record) a send found a later position in its slot
    refused = FALSE;                   \* (record) an upgrade found no sender left

fair process sender \in Senders \union Weak
variables pos = 0, sent = 0, holds = self \in Senders;
begin
S_upgrade:
    \* A weak sender's upgrade, one compare-and-swap on the sender count.
    \* MUTATION upgrade_closed: the upgrade counts a sender with none left.
    if ~holds then
        if live > 0 \/ Mutation = "upgrade_closed" then
            live := live + 1;
            upgraded := upgraded + 1;
            holds := TRUE;
        else
            refused := TRUE;
        end if;
    end if;
S_loop:
    while holds /\ sent < PerSender do
        pos := tail;
        tail := tail + 1;
S_publish:
        \* MUTATION over_later: a send publishes over a later position.
        if slots[pos % Cap] > pos /\ Mutation # "over_later" then
            superseded := TRUE;
        else
            slots[pos % Cap] := pos;
        end if;
        sent := sent + 1;
    end while;
S_gone:
    if holds then
        live := live - 1;
        if live = 0 then
            closed := TRUE;
        end if;
    end if;
end process;

fair process receiver = ReceiverT
variables seen = -1;
begin
R_loop:
    while ~done do
        seen := slots[next % Cap];
R_act:
        if seen = next then
            taken := taken + 1;
            next := next + 1;
        elsif seen > next then
            \* The oldest position the ring can still hold, from the tail now.
            \* MUTATION lag_uncounted: the skipped values go uncounted.
            if Mutation # "lag_uncounted" then
                lost := lost + (tail - Cap - next);
            end if;
            lagged := TRUE;
            next := tail - Cap;
        \* MUTATION closed_only: the receiver is done once the channel is
        \* closed, with values still in the ring.
        elsif closed /\ (tail <= next \/ Mutation = "closed_only") then
            done := TRUE;
        end if;
    end while;
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

\* Every value the senders send: the senders', and those of each upgrade.
Total == (Cardinality(Senders) + upgraded) * PerSender

\* Once done, what the receiver took plus what it heard it lost is every value sent.
Counted == done => taken + lost = Total

\* The receiver's position only moves forward, and never past the tail.
Forward == next <= tail

\* The receiver reaches the close.
Finishes == <>done

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The receiver hears of a lag.
NoLag == ~lagged
\* A send finds a later position in its slot, and its value goes.
NoSuperseded == ~superseded
\* A weak sender's upgrade makes a sender.
NoUpgrade == upgraded = 0
\* A weak sender's upgrade finds no sender left.
NoRefused == ~refused
=============================================================================
