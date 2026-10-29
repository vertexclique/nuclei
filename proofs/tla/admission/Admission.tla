---------------------------- MODULE Admission ----------------------------
(***************************************************************************)
(* How a shard's ring lets operations into the kernel                      *)
(* (src/rt/shard/ring.rs: `ShardRing::new` sizes the receive group and the *)
(* cap, `enter`, `admit_waiting` and `counts_against_cap` apply it), one   *)
(* step per event.                                                         *)
(*                                                                         *)
(* In plain words: the ring's completion queue has C slots, where finished *)
(* operations and arrived data wait for the shard to collect them. Two     *)
(* kinds of operation go into the kernel. A long-lived one (a receive that *)
(* waits on a connection, an accept) may sit there for as long as the      *)
(* connection is idle, maybe forever. A finite one (a send, a file call)   *)
(* finishes soon after it is let in. The ring has a receive group of       *)
(* buffers: data that arrives for the long-lived receives takes one slot   *)
(* of the queue per buffer, so the group is a share of the queue that      *)
(* belongs to arrivals. The rest of the queue is the CAP: how many         *)
(* operations the ring lets in at once. A finite operation that finds the  *)
(* cap used waits in a line called room until one completes.               *)
(*                                                                         *)
(* The two guards: (1) the group is cut to the largest power of two within *)
(* half the queue, so arrivals never take more than half of it and the cap *)
(* never collapses; (2) long-lived operations are let in at once and do    *)
(* not count against the cap, so idle connections cannot use it up.        *)
(*                                                                         *)
(* What must never happen: (a) a finite operation waits in room while no   *)
(* finite operation is in the kernel. Nothing then ever completes to free  *)
(* room, because the long-lived ones may never end: the request waits for  *)
(* its reply's receive, which waits for the request. (b) arrivals plus     *)
(* finite completions take more than the queue's C slots: the room the cap *)
(* keeps is for them.                                                      *)
(*                                                                         *)
(* What is modelled: the queue size, the group size, the cap, long-lived   *)
(* operations that never end by themselves, finite operations that come    *)
(* and complete, the line of room, data arriving in the group and being    *)
(* taken. What is abstract: what the operations do, time, and the entries  *)
(* no room is kept for: an accept's connections and the last entry of each *)
(* long-lived operation. Past the queue the kernel holds those             *)
(* (IORING_FEAT_NODROP, which the poller requires) until the shard takes   *)
(* them: none is lost, and none holds up a finite operation.               *)
(***************************************************************************)
EXTENDS Integers

CONSTANTS
    C,         \* completion queue slots of the ring
    G,         \* receive buffers the configuration asks for (a power of two)
    MaxLong,   \* long-lived operations the environment may start
    MaxFin,    \* finite operations the environment may start
    Mode       \* "ok", or the name of the one rule this run takes away

Min(a, b) == IF a < b THEN a ELSE b
Max(a, b) == IF a > b THEN a ELSE b

\* The largest power of two that is at most half of the queue (for a queue of at least 2).
FitPow == CHOOSE p \in {1, 2, 4, 8, 16, 32, 64} : p <= C \div 2 /\ 2 * p > C \div 2

\* The buffers the ring's group really gets.
Group ==
    IF Mode = "no_group_fit" THEN G
    ELSE IF C < 2 THEN 0
    ELSE Min(G, FitPow)

\* The most operations the ring counts in the kernel: never below one.
Cap == Max(1, C - Group)

(* --algorithm Admission
variables
    long = 0,      \* long-lived operations in the kernel
    fin = 0,       \* finite operations in the kernel
    room = 0,      \* finite operations waiting for room
    made = 0,      \* finite operations started so far
    arr = 0;       \* arrivals in the queue, one per group buffer in use

define
    \* Operations the ring counts against the cap.
    Counted == fin + (IF Mode = "no_long_exemption" THEN long ELSE 0)
    \* No finite operation waits while none is in the kernel to free room.
    NoRoomDeadlock == ~(room > 0 /\ fin = 0)
    \* The queue holds no more waiting entries than it has slots.
    CqNeverOverfull == arr + fin <= C
    \* Witnesses.
    NoRoomEver == room = 0
    LongWithinCap == long <= Cap
end define;

fair process ring = 1
begin
Step:
    while TRUE do
        either
            \* A long-lived operation is armed. With the exemption it goes in at
            \* once; without it, it counts and goes in only while the cap has room.
            await long < MaxLong /\ (Mode /= "no_long_exemption" \/ Counted < Cap);
            long := long + 1;
        or
            \* A finite operation comes: in if the cap has room, else into room.
            await made < MaxFin;
            made := made + 1;
            if Counted < Cap then
                fin := fin + 1;
            else
                room := room + 1;
            end if;
        or
            \* A finite operation completes: the freed slot goes to the one that
            \* has waited longest, if any.
            await fin > 0;
            if room > 0 then
                room := room - 1;
            else
                fin := fin - 1;
            end if;
        or
            \* Data arrives for a long-lived receive, in a free buffer of the group.
            await long > 0 /\ arr < Group;
            arr := arr + 1;
        or
            \* A reader takes an arrival and the buffer goes back to the group.
            await arr > 0;
            arr := arr - 1;
        end either;
    end while;
end process;
end algorithm; *)
=============================================================================
