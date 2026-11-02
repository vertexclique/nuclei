-------------------------------- MODULE Stash --------------------------------
(***************************************************************************)
(* A receive that hands its slot back while it holds the value it took     *)
(* (src/sync/mpsc/chan.rs: `Chan::next` and `Chan::try_recv` through      *)
(* `Chan::received`, whose `Held` guard keeps the value across             *)
(* `Limit::release`, and `Pop::Stashed`;                                   *)
(* src/sync/mpsc/queue.rs: `MessageQueue::put_back` and `Popped::Stashed`; *)
(* the divert in src/sync/mpsc/queue/ring.rs and radix.rs `try_pop`).      *)
(*                                                                          *)
(* In plain words: one sender sends the values 1, 2, 3 in order through a  *)
(* channel of Cap slots: each send takes a free slot (waiting while none  *)
(* is), then pushes its value. The receiver takes the value at the head of *)
(* the queue and gives its slot back. Giving the slot back serves a send   *)
(* that waits for one and wakes it, and that waker is user code that may   *)
(* panic: then the receive unwinds with the value still in its hand. A     *)
(* guard puts the value back ahead of every other (the stash, which the    *)
(* queue hands out first), and the receiver's caller receives again: the   *)
(* stashed value comes out with no slot given back for it, its slot having *)
(* gone back already.                                                      *)
(*                                                                          *)
(* What must never happen: a value lost, received twice or out of order;   *)
(* a slot lost or made up.                                                 *)
(*                                                                          *)
(* What is abstract: the slots' semaphore is a count, and a waiting send   *)
(* takes a slot once one is free (the bounded channel's model checks its   *)
(* line); the receiver's wait for a value is an await.                     *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Cap,       \* the channel's slots
    Values,    \* how many values the sender sends, 1, 2, ... in order
    Panics,    \* how many slot give-backs meet a waker that panics
    Mutation   \* "none", or the name of the one rule this run takes away

None == 0

(* --algorithm Stash
variables
    free = Cap,              \* free slots
    queue = <<>>,            \* the values queued, oldest first
    reserved = 0,            \* slots the sender holds and has not pushed into yet
    stash = None,            \* a value put back ahead of the queue, its slot already back
    received = <<>>,         \* (record) the values the receiver handed out, in order
    lost = 0,                \* (record) values taken and dropped
    panicsLeft = Panics;     \* give-backs still to meet a panicking waker

define
    \* Every value is received once, in the order it was sent.
    InOrder == \A i \in 1..Len(received) : received[i] = i
    \* Nothing taken is ever dropped.
    NoValueLost == lost = 0
    \* Every slot is free, reserved, holds a queued value, or holds the value the receiver has
    \* taken and not given its slot back for yet. A stashed value holds none.
    SlotsConserved ==
        free + reserved + Len(queue) + (IF pc["rx"] = "R_back" THEN 1 ELSE 0) = Cap
    \* Every value sent is received.
    AllReceived == <>(Len(received) = Values)
end define;

fair process sender = "tx"
variables v = 1;
begin
S_next:
    while v <= Values do
S_reserve:
        \* A free slot, taken (it waits while none is).
        await free > 0;
        free := free - 1;
        reserved := reserved + 1;
S_push:
        \* The value pushed into the slot.
        queue := Append(queue, v);
        reserved := reserved - 1;
        v := v + 1;
    end while;
end process;

fair process receiver = "rx"
variables held = None;
begin
R_take:
    while Len(received) + lost < Values do
        \* The queue's look: the stash first (the divert), then the head. A stashed value is
        \* handed out with no slot given back. MUTATION stash_released_again: its slot is given
        \* back once more.
        await stash # None \/ queue # <<>>;
        if stash # None then
            received := Append(received, stash);
            stash := None;
            if Mutation = "stash_released_again" then
                free := free + 1;
            end if;
        else
            held := Head(queue);
            queue := Tail(queue);
R_back:
            \* The slot goes back (`Limit::release`): a waiting send may take it at once.
            free := free + 1;
R_wake:
            \* The give-back's pass wakes the send it served. A waker that panics unwinds the
            \* receive: the guard stashes the value, and the receiver's caller receives again.
            \* MUTATION pop_lost: no guard; the value is dropped with the unwind.
            if panicsLeft > 0 then
                panicsLeft := panicsLeft - 1;
                if Mutation = "pop_lost" then
                    lost := lost + 1;
                else
                    stash := held;
                end if;
            else
                received := Append(received, held);
            end if;
            held := None;
        end if;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
