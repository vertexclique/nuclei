---------------------------- MODULE Admission ----------------------------
(***************************************************************************)
(* How a shard's ring lets operations into the kernel                      *)
(* (src/rt/shard/ring.rs: `ShardRing::new` sizes the receive group and the *)
(* cap, `submit`, `submit_file`, `enter` and `admit_waiting` apply it, and *)
(* `zero_copy` ends a zero-copy send's first part; src/rt/shard/ring/      *)
(* exits.rs `watch_exit`), one step per event.                             *)
(*                                                                         *)
(* In plain words: the ring's completion queue has C slots, where finished *)
(* operations and arrived data wait for the shard to collect them. Two     *)
(* kinds of operation go into the kernel. A long-lived one, every          *)
(* operation of a source, may sit there for as long as its peer pleases,   *)
(* maybe forever: a side that reads (a receive of any kind, multishot or   *)
(* into one buffer, an accept, a poll for bytes) waits for its peer to     *)
(* send or connect, and a side that writes (a send, a poll for room, a     *)
(* connect under way, a zero-copy send that holds its buffer until the     *)
(* kernel's notification) for its peer to read, answer or acknowledge,     *)
(* which a peer that never reads never does. A child's exit watch waits    *)
(* for the child. A multishot one never ends by itself; a one-shot one     *)
(* ends with its peer's one act, and its task arms the next. A finite one, *)
(* a file's read or write, finishes soon after it is let in, whoever is at *)
(* the other end of a socket. The ring has a receive group of buffers:     *)
(* data that arrives for the long-lived receives takes one slot of the     *)
(* queue per buffer, so the group is a share of the queue that belongs to  *)
(* arrivals. The rest of the queue is the CAP: how many operations the     *)
(* ring lets in at once. A finite operation that finds the cap used waits  *)
(* in a line called room until one completes.                              *)
(*                                                                         *)
(* The two guards: (1) the group is cut to the largest power of two within *)
(* half the queue, so arrivals never take more than half of it and the cap *)
(* never collapses; (2) long-lived operations, one-shot ones and a write   *)
(* side's too, are let in at once and do not count against the cap, so     *)
(* idle or slow peers cannot use it up. Before, the ring counted a         *)
(* one-shot receive, poll or accept as finite: 3840 idle connections (the  *)
(* cap of a ring of 2048 entries beside its 256-buffer group) filled it    *)
(* with receives that waited for their peers, and the next accept waited   *)
(* in room behind them for good, the stall a many-connections benchmark    *)
(* found at                                                                *)
(* 4096 connections. It counted every write side's operation too: sends to *)
(* peers that never read could fill the cap the same way, and hold every   *)
(* other connection's reply, and every file call, in room for good.        *)
(*                                                                         *)
(* What must never happen: (a) a finite operation waits in room while no   *)
(* finite operation is in the kernel. Nothing then ever completes to free  *)
(* room, because the long-lived ones may never end: the request waits for  *)
(* its reply's receive, which waits for the request. (b) arrivals plus     *)
(* finite completions take more than the queue's C slots: the room the cap *)
(* keeps is for them.                                                      *)
(*                                                                         *)
(* A side that reads by readiness (a stream read straight into the task's  *)
(* buffer, a datagram socket whose task receives itself) keeps a multishot *)
(* poll armed (`ReadyMulti`, `ReadyStream` in src/io/source/read/ops.rs),  *)
(* and each of its edges is an entry of the queue; the ring's own cancels  *)
(* (`ShardRing::cancel`) each complete with one too. Neither has room kept: *)
(* their number follows the sides and their peers, not the cap, so the     *)
(* queue may need more than its C slots for them, and the kernel holds the *)
(* rest past it as it holds an accept's connections. A multishot poll's    *)
(* edge that finds the queue full ends the poll instead (Linux posts the   *)
(* edges of a multishot operation only into room, and ends it otherwise:   *)
(* `Kernel = "ends"`; kernels before that held them past the queue too:    *)
(* `"overflows"`). Its end wakes the side's tasks, and the next one that   *)
(* waits arms another (`ReadyMultiEnd` in `ReadState::op_done`), whose     *)
(* first look at the socket reports bytes already there, so no edge is     *)
(* lost. What must happen: a poll the kernel ended is armed again          *)
(* (`EndedRearmed`).                                                       *)
(*                                                                         *)
(* What is modelled: the queue size, the group size, the cap, multishot    *)
(* long-lived operations that never end by themselves, one-shot ones of    *)
(* each side that end when their peer acts, finite operations that come    *)
(* and complete, the line of room, data arriving in the group and being    *)
(* taken, the multishot polls' edges, their end on a full queue and their  *)
(* arming again, the ring's cancels, and the entries the kernel holds past *)
(* the queue. What is abstract: what the operations do, time, and the entries  *)
(* no room is kept for: an accept's connections and the end of each        *)
(* long-lived operation. Past the queue the kernel holds those             *)
(* (IORING_FEAT_NODROP, which the poller requires) until the shard takes   *)
(* them: none is lost, and none holds up a finite operation                *)
(* (tests/cq_overflow.rs ends many times the queue's entries at once).     *)
(*                                                                         *)
(* With `AllocFails` set, the kernel's memory for an entry it holds past   *)
(* the queue may not be had (its allocation is `GFP_NOWAIT` or charged to  *)
(* a memory cgroup at its limit): the kernel drops the entry, counts it    *)
(* (the rings' `cq.overflow`), and its next wait for completions answers   *)
(* `EBADR` once, after which the ring works again. The shard takes that    *)
(* answer as news, not as its end: it counts what the kernel dropped since *)
(* it last looked (src/rt/park/ring.rs, the poller's wait), says it at     *)
(* error, and goes on (`ShardLives`, `DropsCounted`). Before, the shard    *)
(* panicked (`panic_on_dropped`). It also recovers (src/rt/shard/ring/     *)
(* recover.rs): it cancels every operation it still has out, and each      *)
(* still in the kernel ends with that cancel, its side arming another;     *)
(* then it sweeps every operation whose end never came, ending it for its  *)
(* side with an error that says the kernel dropped it, so a poll whose end *)
(* was dropped is armed again too (`DroppedRecovered`). The cancel comes   *)
(* first so that the sweep finds only what the kernel no longer holds      *)
(* (`NoFalseSweep`). In the code the sweep comes a bounded time after the  *)
(* cancel (a socket's operation and a child's exit watch 1 s, a file's     *)
(* call, which ignores a cancel once the kernel runs it, the runtime's     *)
(* linger time); one that outlasts it is ended with the error and keeps    *)
(* its memory until its own end, if that ever comes, so a late end never   *)
(* touches freed memory. A zero-copy send that only holds its buffer until *)
(* its peer acknowledges the bytes is not swept: no cancel ends that wait, *)
(* which may last for good.                                                *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS
    C,         \* completion queue slots of the ring
    G,         \* receive buffers the configuration asks for (a power of two)
    MaxLong,   \* long-lived operations of each kind the environment may start
    MaxFin,    \* finite operations the environment may start
    Polls,     \* multishot polls of sides that read by readiness, armed from the start
    MaxEdges,  \* edges the polls bring over the run
    MaxCancel, \* cancels the ring makes of its own
    Kernel,    \* "ends": a multishot edge that finds the queue full ends its poll; "overflows": it is held past the queue
    AllocFails, \* TRUE: an entry the kernel would hold past the queue may be dropped instead, for want of memory
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
    long = 0,      \* multishot long-lived operations in the kernel
    once = 0,      \* one-shot long-lived operations in the kernel, each waiting on its peer
    sends = 0,     \* a write side's operations in the kernel, each waiting for its peer to read
    fin = 0,       \* finite operations in the kernel
    room = 0,      \* finite operations waiting for room
    made = 0,      \* finite operations started so far
    arr = 0,       \* arrivals in the queue, one per group buffer in use
    ps = [i \in 1..Polls |-> "armed"], \* each poll: "armed" (in the kernel), "ended" (its end reached its side,
                                     \* which arms another), "lost" (its end dropped: still in the ring's table)
    pedges = 0,    \* poll edges in the queue
    brought = 0,   \* edges the polls brought so far
    cancels = 0,   \* the ring's cancels' completions in the queue
    cancelled = 0, \* cancels the ring made so far
    over = 0,      \* entries the kernel holds past the queue (`IORING_FEAT_NODROP`)
    dropped = 0,   \* entries the kernel dropped for want of memory (its `cq.overflow` count)
    unsaid = FALSE, \* the kernel dropped one and its waits have not answered `EBADR` for it yet
    counted = 0,   \* entries the shard counted as dropped
    alive = TRUE,  \* the shard runs
    recovering = FALSE, \* a drop was said: the shard cancels what is out and then sweeps
    snap = [i \in 1..Polls |-> FALSE], \* the poll is one of those out when the drop was said (the sweep's)
    recCancel = FALSE,  \* the recovery's cancel went in: every poll then in the kernel ended
    falseSweep = FALSE; \* (record) the sweep ended a poll the kernel still held

define
    \* Operations the ring counts against the cap.
    \* MUTATION once_counted: one-shot receives, polls and accepts count, as they did before.
    \* MUTATION sends_counted: a write side's operations count, as they did before.
    Counted == fin + (IF Mode = "no_long_exemption" THEN long ELSE 0)
                   + (IF Mode = "once_counted" THEN once ELSE 0)
                   + (IF Mode = "sends_counted" THEN sends ELSE 0)
    \* Entries in the queue now, the finite operations counted as if they all completed.
    Used == arr + fin + pedges + cancels
    \* No finite operation waits while none is in the kernel to free room.
    NoRoomDeadlock == ~(room > 0 /\ fin = 0)
    \* The queue holds no more waiting entries than it has slots.
    CqNeverOverfull == arr + fin <= C
    \* Witnesses.
    NoRoomEver == room = 0
    LongWithinCap == long <= Cap
    OnceWithinCap == once <= Cap
    SendsWithinCap == sends <= Cap
    \* Witness: the queue never needs more than its slots, so the kernel holds nothing past it.
    NoOverflow == over = 0
    \* The shard never ends for a completion the kernel dropped.
    ShardLives == alive
    \* The sweep never ends an operation the kernel still holds.
    NoFalseSweep == ~falseSweep
    \* Polls in the kernel, ended and waiting for their side to arm another, and lost.
    Armed == {i \in 1..Polls : ps[i] = "armed"}
    Ended == Cardinality({i \in 1..Polls : ps[i] = "ended"})
    Lost == Cardinality({i \in 1..Polls : ps[i] = "lost"})
end define;

fair process ring = 1
begin
Step:
    while alive do
        either
            \* A long-lived operation is armed. With the exemption it goes in at
            \* once; without it, it counts and goes in only while the cap has room.
            await long < MaxLong /\ (Mode /= "no_long_exemption" \/ Counted < Cap);
            long := long + 1;
        or
            \* A one-shot long-lived operation is armed (a receive into a buffer, a poll for
            \* bytes, an accept). With the exemption it goes in at once; counted, it goes in
            \* only while the cap has room.
            await once < MaxLong /\ (Mode /= "once_counted" \/ Counted < Cap);
            once := once + 1;
        or
            \* Its peer sends or connects: it ends, and its reader may arm the next.
            await once > 0;
            once := once - 1;
        or
            \* A write side's operation is armed (a send, a poll for room, a connect). With the
            \* exemption it goes in at once; counted, it goes in only while the cap has room.
            await sends < MaxLong /\ (Mode /= "sends_counted" \/ Counted < Cap);
            sends := sends + 1;
        or
            \* Its peer reads, or answers: it ends, and its task may arm the next.
            await sends > 0;
            sends := sends - 1;
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
        or
            \* An armed poll's socket became ready: its edge goes into the queue if there is room. A full
            \* queue ends the poll (its end, which the kernel holds past the queue, wakes the side), or on
            \* an older kernel holds the edge past the queue.
            await Armed # {} /\ brought < MaxEdges;
            brought := brought + 1;
            if Used < C then
                pedges := pedges + 1;
            elsif Kernel = "ends" then
                \* The poll's end is held past the queue, or with `AllocFails` dropped: then it is lost,
                \* still in the ring's table.
                with i \in Armed do
                    either
                        ps[i] := "ended";
                        snap[i] := FALSE;
                    or
                        await AllocFails;
                        ps[i] := "lost";
                        dropped := dropped + 1;
                        unsaid := TRUE;
                    end either;
                end with;
            else
                \* The edge is held past the queue, or with `AllocFails` dropped.
                either
                    over := over + 1;
                or
                    await AllocFails;
                    dropped := dropped + 1;
                    unsaid := TRUE;
                end either;
            end if;
        or
            \* The shard takes a poll edge: the side reads.
            await pedges > 0;
            pedges := pedges - 1;
        or
            \* The ring cancels one of its operations: the cancel's completion goes into the queue, or past it.
            await cancelled < MaxCancel;
            cancelled := cancelled + 1;
            if Used < C then
                cancels := cancels + 1;
            else
                \* Held past the queue, or with `AllocFails` dropped.
                either
                    over := over + 1;
                or
                    await AllocFails;
                    dropped := dropped + 1;
                    unsaid := TRUE;
                end either;
            end if;
        or
            \* The shard takes a cancel's completion, which it ignores.
            await cancels > 0;
            cancels := cancels - 1;
        or
            \* The queue has room again: the shard takes an entry the kernel held past it.
            await over > 0 /\ Used < C;
            over := over - 1;
        end either;
    end while;
end process;

\* The shard's wait for completions: it gets `EBADR`, once, for what the kernel dropped, counts what
\* the kernel's count says was dropped since it last looked, says it, and goes on.
fair process wait = 3
begin
W_report:
    while alive do
        await unsaid \/ ~alive;
        if alive then
            unsaid := FALSE;
            \* MUTATION panic_on_dropped: the shard ends on the error (the code before).
            \* MUTATION drop_uncounted: the error is passed over and nothing counted.
            \* MUTATION no_recovery: it counts and goes on, and recovers nothing (the code before).
            if Mode = "panic_on_dropped" then
                alive := FALSE;
            elsif Mode # "drop_uncounted" then
                counted := dropped;
                if Mode # "no_recovery" then
                    \* The operations out now are the sweep's: those in the kernel, and those whose end
                    \* was dropped.
                    recovering := TRUE;
                    recCancel := FALSE;
                    snap := [i \in 1..Polls |-> snap[i] \/ ps[i] \in {"armed", "lost"}];
                end if;
            end if;
        end if;
    end while;
end process;

\* The shard's recovery: it cancels every operation it still has out, then sweeps what is left.
fair process recover = 4
begin
V_loop:
    while alive do
        await recovering \/ ~alive;
        if alive then
            either
                \* The cancel: every poll still in the kernel ends with it, and its side arms another.
                \* MUTATION no_cancel: the shard sweeps without cancelling first.
                await ~recCancel /\ Mode # "no_cancel";
                with keep = [i \in 1..Polls |-> snap[i] /\ ps[i] = "lost"] do
                    ps := [i \in 1..Polls |-> IF snap[i] /\ ps[i] = "armed" THEN "ended" ELSE ps[i]];
                    snap := keep;
                end with;
                recCancel := TRUE;
            or
                \* The sweep, once the cancel's ends were taken: every operation whose end never came
                \* ends for its side with an error saying the kernel dropped it, and its side arms
                \* another; a poll the kernel still held would be ended in its place by mistake.
                \* MUTATION skip_sweep: the shard cancels and sweeps nothing.
                await recCancel \/ Mode = "no_cancel";
                if \E i \in 1..Polls : snap[i] /\ ps[i] = "armed" then
                    falseSweep := TRUE;
                end if;
                if Mode # "skip_sweep" then
                    ps := [i \in 1..Polls |-> IF snap[i] THEN "ended" ELSE ps[i]];
                end if;
                snap := [i \in 1..Polls |-> FALSE];
                recovering := FALSE;
            end either;
        end if;
    end while;
end process;

\* The sides whose poll the kernel ended: the end wakes their tasks, and the next to wait arms another.
fair process sides = 2
begin
S_rearm:
    while alive do
        \* MUTATION no_rearm_after_full: an end that came for want of room is taken as the side's last word.
        await Ended > 0 /\ Mode /= "no_rearm_after_full";
        with i \in {j \in 1..Polls : ps[j] = "ended"} do
            ps[i] := "armed";
        end with;
    end while;
end process;
end algorithm; *)
\* BEGIN TRANSLATION
\* END TRANSLATION

\* Every poll the kernel ended for want of room is armed again.
EndedRearmed == (Ended > 0) ~> (Ended = 0)

\* Every entry the kernel dropped is counted by the shard in the end.
DropsCounted == (dropped > counted) ~> (dropped = counted)

\* Every poll whose end the kernel dropped is ended for its side in the end, which arms another.
DroppedRecovered == (Lost > 0) ~> (Lost = 0)
=============================================================================
