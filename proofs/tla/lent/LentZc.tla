------------------------------- MODULE LentZc -------------------------------
(***************************************************************************)
(* A zero-copy stream write, and the buffer it hands back later.           *)
(*                                                                          *)
(* The Rust it models: `TcpStream::write_zero_copy` (src/net/stream.rs),   *)
(* `Source::write_zero_copy` and `WriteState::write_owned`                  *)
(* (src/io/source/write/calls.rs, src/io/source/write.rs), `lent_want`,    *)
(* `submit_ops` and `lent_back` (src/io/source/write/ops.rs), `Pinned`      *)
(* (src/io/source/write/pinned.rs), `Released` (src/net/released.rs), and  *)
(* on the ring's thread `ShardRing::zero_copy` and `ShardRing::shut_down`  *)
(* (src/rt/shard/ring.rs) with `entry` (src/io/source/ring.rs).            *)
(*                                                                          *)
(* In plain words: a task writes buffer A (units 1 2 3) and then buffer B  *)
(* (unit 4) to a TCP stream, each with a zero-copy send, and then reads    *)
(* what its peer wrote. The side sends a buffer in chunks of at most two   *)
(* units, one chunk at a time. For each chunk the kernel takes some units  *)
(* into the socket (all it has room for, or fewer: a short count) and      *)
(* posts a first completion; the side counts them and sends the next chunk *)
(* from where the kernel stopped. The kernel goes on reading the task's    *)
(* memory until it lets go of every unit it took (here: once the peer read *)
(* it, or earlier if the network acknowledged it), and then posts that     *)
(* chunk's notification. The write ends for the task once every unit was  *)
(* counted, so the task writes B while A may still be pinned, and reads.   *)
(* The buffer comes back through its Released once the notification of    *)
(* every chunk came; the task may drop the Released, and then the runtime  *)
(* drops the buffer at the last notification. The peer writes to the task  *)
(* before it reads anything. The task may go away while A is lent: a write *)
(* none of which went out is withdrawn, one that started is finished. A    *)
(* send may fail, which breaks the socket for good. The shard's ring may   *)
(* go while notifications are still out: what its operations hold is kept  *)
(* for good.                                                               *)
(*                                                                          *)
(* What must never happen: units on the wire out of order, twice, or a     *)
(* withdrawn buffer partly on it (WireOrder); a buffer handed back or      *)
(* freed while the kernel may still read it (NoScribble); and the exchange *)
(* must end (Finishes), the write without the peer reading first.          *)
(*                                                                          *)
(* What is abstract: the side's guard (a step under it runs whole here;    *)
(* Side.tla checks it), a send cancelled as its source moves (RingLent.tla *)
(* checks it), the copied writes and their order (LentWrite.tla), and the  *)
(* size below which a write is copied, which is a cost choice, not a rule. *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Room,      \* units the socket holds between the task's send and the peer's reads
    PeerLen,   \* units the peer writes to the task before it reads any (0: it only reads)
    Room2,     \* units of the peer's write the task's end of the socket holds
    MayLeave,  \* whether the task may go away while buffer A is lent
    MayDrop,   \* whether the task may drop a Released instead of waiting for it
    MayFail,   \* whether a send may fail, which breaks the socket for good
    Teardown,  \* whether the shard's ring may go while operations are still out
    Mutation   \* "none", or the name of the one rule this run takes away

Bufs == {"A", "B"}                                    \* the task's two buffers
Start == [b \in Bufs |-> IF b = "A" THEN 1 ELSE 4]    \* the first unit of each buffer on the stream
Stop == [b \in Bufs |-> IF b = "A" THEN 3 ELSE 4]     \* the last unit of each buffer on the stream
Size(b) == Stop[b] - Start[b] + 1                     \* how many units a buffer has
Chunk == 2                                            \* the most units one send carries (`SEND_ZC_CHUNK`)
Whole == <<1, 2, 3, 4>>                               \* the stream once both writes went out whole
MaxOps == 6                                           \* more sends than a run makes, for fairness
Min(a, b) == IF a < b THEN a ELSE b                   \* the smaller of two numbers
IsPrefix(s, t) == Len(s) <= Len(t) /\ SubSeq(t, 1, Len(s)) = s  \* s is how t begins
Range(s) == {s[k] : k \in DOMAIN s}                   \* the units a sequence holds
Want(o) == o.to - o.from + 1                          \* how many units a send asked the kernel to take

VARIABLES
    task,       \* where the task is: "lendA" "waitA" "lendB" "waitB" "read" "relA" "relB" "done" "gone"
    rx,         \* each buffer's Released: "none" (not made yet), "alive" (the task has it), "dropped"
    tread,      \* units of the peer's write the task read
    cur,        \* the buffer the side is writing now, or "none"
    sent,       \* units of that buffer the side counted from first completions
    out,        \* the index of the side's send in `ops` that is out (0: none)
    orphan,     \* the task went away after some of A went out: the side finishes A
    withdrawn,  \* A was withdrawn: none of it went out
    lent,       \* each buffer: the side's lent write still holds it
    wdone,      \* each buffer: its write ended for the task
    bufst,      \* each buffer: "task", "lent" (shared), "handed" (back to the task), "freed"
    ops,        \* every send, in order: what it asked for, what it took, its completions
    wire,       \* units the kernel took from sends, in order: the stream the peer gets
    kfree,      \* units the kernel let go of: the peer read them, or the network acked them
    got,        \* units the peer read
    pwrote,     \* units the peer wrote to the task
    back,       \* units the peer wrote that the task has not read
    broken,     \* a send failed: the socket takes nothing more
    ringGone    \* the shard's ring went: its thread applies no completion again

vars == <<task, rx, tread, cur, sent, out, orphan, withdrawn, lent, wdone, bufst,
          ops, wire, kfree, got, pwrote, back, broken, ringGone>>  \* every variable, for stuttering

sideVars == <<cur, sent, out, orphan, withdrawn, lent, wdone>>   \* what the write side holds
peerVars == <<got, pwrote, back>>                               \* what the peer has done

(***************************************************************************)
(* Who holds a buffer, and when it comes back.                             *)
(***************************************************************************)

\* The kernel may still read buffer b: a send of it has not completed yet, or units it took are
\* still in the kernel's hands.
KernelReads(b) == \E i \in DOMAIN ops :
                      /\ ops[i].buf = b                                  \* a send of this buffer
                      /\ \/ ops[i].first = "no"                          \* still in the kernel
                         \/ ~(Range(ops[i].took) \subseteq kfree)        \* or its units still held

\* How many hold buffer b, given the side's lent flag l and the sends o: the lent write, and every
\* send that has not let go of it yet (each keeps it until its notification).
Holds(b, l, o) == (IF l[b] THEN 1 ELSE 0)                                 \* the side's lent write
                  + Cardinality({i \in DOMAIN o : o[i].buf = b /\ o[i].held})  \* and each send

\* What buffer b becomes once the holders are l and o and its Released is r: when the last holder
\* lets go of a shared buffer, it goes to a Released still alive, or else it is freed.
Settle(b, l, o, r) == IF bufst[b] = "lent" /\ Holds(b, l, o) = 0          \* the last let go
                      THEN IF r[b] = "alive" THEN "handed" ELSE "freed"   \* back, or dropped
                      ELSE bufst[b]                                       \* otherwise as it was

\* Every buffer settled, for the holders l, sends o and Released r of the step.
SettleAll(l, o, r) == [c \in Bufs |-> Settle(c, l, o, r)]

Init ==
    /\ task = "lendA"                       \* the task is about to lend A
    /\ rx = [b \in Bufs |-> "none"]         \* no Released exists yet
    /\ tread = 0                            \* the task read nothing
    /\ cur = "none"                         \* the side writes nothing
    /\ sent = 0                             \* and counted nothing
    /\ out = 0                              \* no send is out
    /\ orphan = FALSE                       \* the task is there
    /\ withdrawn = FALSE                    \* nothing was withdrawn
    /\ lent = [b \in Bufs |-> FALSE]        \* the side holds no buffer
    /\ wdone = [b \in Bufs |-> FALSE]       \* no write ended
    /\ bufst = [b \in Bufs |-> "task"]      \* the task has both buffers
    /\ ops = <<>>                           \* no send was made
    /\ wire = <<>>                          \* the stream is empty
    /\ kfree = {}                           \* the kernel holds no unit
    /\ got = 0                              \* the peer read nothing
    /\ pwrote = 0                           \* the peer wrote nothing
    /\ back = 0                             \* nothing waits for the task to read
    /\ broken = FALSE                       \* the socket works
    /\ ringGone = FALSE                     \* the ring is there

(***************************************************************************)
(* The task (`write_zero_copy`, then its reads, then the Releaseds).       *)
(***************************************************************************)

\* The task lends buffer b to the side: the write starts (`WriteState::write_owned`).
Lend(b, at, next) ==
    /\ task = at                                      \* the task is about to write b
    /\ task' = next                                   \* and then waits for the write
    /\ lent' = [lent EXCEPT ![b] = TRUE]              \* the side's lent write holds b
    /\ bufst' = [bufst EXCEPT ![b] = "lent"]          \* the buffer is out of the task's hands
    /\ cur' = b                                       \* the side writes b
    /\ sent' = 0                                      \* nothing of it counted yet
    /\ UNCHANGED <<rx, tread, out, orphan, withdrawn, wdone, ops, wire, kfree, peerVars,
                   broken, ringGone>>                 \* nothing else changes

\* The write of b ended (every unit counted, or it failed): the task's poll takes the lent write,
\* lets go of its hold, and gets the Released (`Released::waiting`).
\* MUTATION hand_at_first: the buffer comes back as the write ends, before the notifications.
Take(b, at, next) ==
    /\ task = at                                      \* the task waits for the write of b
    /\ wdone[b]                                       \* which ended
    /\ task' = next                                   \* the task goes on
    /\ lent' = [lent EXCEPT ![b] = FALSE]             \* the lent write lets go of b
    /\ rx' = [rx EXCEPT ![b] = "alive"]               \* the task holds b's Released
    /\ cur' = "none"                                  \* the side writes nothing now
    /\ bufst' = IF Mutation = "hand_at_first"         \* the planted bug: back at once
                THEN [bufst EXCEPT ![b] = "handed"]
                ELSE SettleAll(lent', ops, rx')       \* back only if no send holds it
    /\ UNCHANGED <<tread, sent, out, orphan, withdrawn, wdone, ops, wire, kfree, peerVars,
                   broken, ringGone>>                 \* nothing else changes

\* The task goes away while A is lent (its write future dropped: `abandon_lent`). A write none of
\* which went out is withdrawn: at once if no send is out, else the send out is cancelled and its
\* completion withdraws it if it took nothing (`withdrawal`). One that started is left to the side.
\* MUTATION withdraw_started: a write some of which went out is withdrawn as well.
Leave ==
    /\ MayLeave                                       \* this run lets the task go away
    /\ task = "waitA"                                 \* while A is lent
    /\ ~wdone["A"]                                    \* and its write has not ended
    /\ task' = "gone"                                 \* the task is gone for good
    /\ LET now == (sent = 0 /\ out = 0)               \* nothing out and nothing counted
                  \/ (Mutation = "withdraw_started" /\ sent > 0)   \* or the planted bug
       IN IF now
          THEN /\ withdrawn' = TRUE                   \* the write is withdrawn
               /\ lent' = [lent EXCEPT !["A"] = FALSE]   \* the side lets go of A
               /\ rx' = [rx EXCEPT !["A"] = "dropped"]   \* with the Released it never handed out
               /\ cur' = "none"                       \* and writes nothing
               /\ out' = 0                            \* and forgets any send out
               /\ bufst' = SettleAll(lent', ops, rx') \* freed if no send holds it
               /\ UNCHANGED <<orphan, ops>>
          ELSE /\ orphan' = TRUE                      \* the side finishes or withdraws later
               /\ ops' = IF out # 0 /\ sent = 0       \* nothing counted and a send out:
                         THEN [ops EXCEPT ![out].cancel = TRUE]  \* ask the kernel to cancel it
                         ELSE ops
               /\ UNCHANGED <<withdrawn, lent, rx, cur, out, bufst>>
    /\ UNCHANGED <<tread, sent, wdone, wire, kfree, peerVars, broken, ringGone>>

\* The task reads one unit of what the peer wrote.
Read ==
    /\ task = "read"                                  \* both writes ended
    /\ back > 0                                       \* and a unit of the peer's waits
    /\ back' = back - 1                               \* the task takes it
    /\ tread' = tread + 1                             \* and counts it
    /\ task' = IF tread + 1 = PeerLen THEN "relA" ELSE "read"  \* all read: on to the Releaseds
    /\ UNCHANGED <<rx, sideVars, bufst, ops, wire, kfree, got, pwrote, broken, ringGone>>

\* The task's Released of b resolves: the buffer is back in its hands.
GetBack(b, at, next) ==
    /\ task = at                                      \* the task waits on b's Released
    /\ bufst[b] = "handed"                            \* which has the buffer
    /\ task' = next                                   \* the task has it, and goes on
    /\ UNCHANGED <<rx, tread, sideVars, bufst, ops, wire, kfree, peerVars, broken, ringGone>>

\* The task drops b's Released instead of waiting for it.
\* MUTATION drop_frees: a dropped Released frees the buffer at once.
Drop(b, at, next) ==
    /\ MayDrop                                        \* this run lets the task drop it
    /\ task = at                                      \* the task holds b's Released
    /\ bufst[b] # "handed"                            \* which has not resolved yet
    /\ task' = next                                   \* the task goes on without it
    /\ rx' = [rx EXCEPT ![b] = "dropped"]             \* nobody waits for b now
    /\ bufst' = IF Mutation = "drop_frees"            \* the planted bug: freed at once
                THEN [bufst EXCEPT ![b] = "freed"]
                ELSE bufst                            \* freed only by the last holder
    /\ UNCHANGED <<tread, sideVars, ops, wire, kfree, peerVars, broken, ringGone>>

AfterB == IF PeerLen = 0 THEN "relA" ELSE "read"     \* where the task goes once B's write ended

(***************************************************************************)
(* The side, on the ring's thread: sends and their completions.            *)
(***************************************************************************)

\* The side sends the next chunk of the buffer it writes, from the first unit not counted yet
\* (`submit_ops`, `lent_want`, `entry`): one send out at a time.
\* MUTATION chunk_early: the next chunk goes out while the previous has not completed, from where
\* that one would end if it took everything.
Submit ==
    /\ ~ringGone                                      \* the ring is there
    /\ cur # "none"                                   \* the side writes a buffer
    /\ Start[cur] + sent <= Stop[cur]                 \* with units not counted yet
    /\ ~wdone[cur]                                    \* and a write that has not ended
    /\ Len(ops) < MaxOps                              \* (a bound no correct run reaches)
    /\ \/ out = 0                                     \* no send of the side is out
       \/ /\ Mutation = "chunk_early"                 \* or the planted bug: one is,
          /\ out # 0                                 \* (the send out)
          /\ ops[out].first = "no"                    \* has not completed,
          /\ ops[out].to < Stop[cur]                  \* and a chunk comes after it
    /\ LET from == IF out = 0 THEN Start[cur] + sent ELSE ops[out].to + 1  \* where it starts
       IN ops' = Append(ops, [buf |-> cur,           \* a send of this buffer
                              from |-> from,          \* from this unit
                              to |-> Min(from + Chunk - 1, Stop[cur]),  \* to the chunk's end
                              took |-> <<>>,          \* nothing taken yet
                              first |-> "no",         \* no first completion yet
                              notif |-> "no",         \* no notification yet
                              cancel |-> FALSE,       \* nobody asked to cancel it
                              failed |-> FALSE,       \* it has not failed
                              held |-> TRUE])         \* it holds the buffer
    /\ out' = Len(ops) + 1                            \* it is the side's send out
    /\ UNCHANGED <<task, rx, tread, cur, sent, orphan, withdrawn, lent, wdone, bufst, wire,
                   kfree, peerVars, broken, ringGone>>

\* The side applies the first completion of send i, whose sends are o after this step: it counts
\* the units taken (`lent_back`). A send cancelled with nothing taken withdraws an abandoned write
\* that has nothing out, and is asked again otherwise. Once every unit was counted, or the send
\* failed, the write ends: for the task, or for nobody if it went (the side then lets go).
\* MUTATION resend_start: a short count sends the buffer again from its start.
Deliver(i, o) ==
    LET b == o[i].buf                                 \* the buffer of the send
        n == Len(o[i].took)                           \* units the kernel took
        short == 0 < n /\ n < Want(o[i])              \* fewer than the send asked for
        s == IF Mutation = "resend_start" /\ short THEN 0 ELSE sent + n  \* units counted now
        mine == b = cur                               \* the side still writes that buffer
        undone == n = 0 /\ ~o[i].failed               \* cancelled before it took anything
        gone == mine /\ undone /\ orphan /\ sent = 0  \* an abandoned write none of which went out
        ends == mine /\ (o[i].failed \/ s = Size(b))  \* the write ends: all counted, or failed
        drop == gone \/ (ends /\ orphan)              \* the side lets go of the lent write
    IN /\ ops' = o                                    \* the sends as this step left them
       /\ out' = IF out = i THEN 0 ELSE out           \* the side's send is no longer out
       /\ sent' = IF mine THEN s ELSE sent            \* the units counted
       /\ withdrawn' = (withdrawn \/ gone)            \* withdrawn, if nothing of it went out
       /\ lent' = IF drop THEN [lent EXCEPT ![b] = FALSE] ELSE lent      \* the hold goes
       /\ rx' = IF drop THEN [rx EXCEPT ![b] = "dropped"] ELSE rx        \* with its Released
       /\ cur' = IF drop THEN "none" ELSE cur         \* the side writes nothing then
       /\ wdone' = IF ends /\ ~orphan THEN [wdone EXCEPT ![b] = TRUE] ELSE wdone  \* task woken
       /\ bufst' = SettleAll(lent', o, rx')           \* freed if that was the last holder
       /\ UNCHANGED orphan                             \* whether the task went stays as it was

\* The ring's thread takes the first completion of send i and hands it to the side at once
\* (`ShardRing::zero_copy`), while the send stays in the ring for its notification.
\* MUTATION wait_notif: the side gets the first completion only with the notification (the bug
\* that shipped: the write waited for the peer's acknowledgment).
First(i) ==
    /\ ~ringGone                                      \* the ring is there
    /\ i \in DOMAIN ops                               \* send i exists
    /\ ops[i].first = "posted"                        \* and the kernel posted its first completion
    /\ LET o == [ops EXCEPT ![i].first = "applied"]   \* the ring took it
       IN IF Mutation = "wait_notif"                  \* the planted bug: the side waits
          THEN /\ ops' = o
               /\ UNCHANGED <<sideVars, rx, bufst>>
          ELSE Deliver(i, o)                          \* the side counts it now
    /\ UNCHANGED <<task, tread, wire, kfree, peerVars, broken, ringGone>>

\* The ring's thread takes the notification of send i: the send lets go of the buffer, and the
\* last holder hands it back or frees it (`Pinned`'s drop).
Notif(i) ==
    /\ ~ringGone                                      \* the ring is there
    /\ i \in DOMAIN ops                               \* send i exists
    /\ ops[i].notif = "posted"                        \* the kernel posted its notification
    /\ ops[i].first = "applied"                       \* after its first completion
    /\ LET o == [ops EXCEPT ![i].notif = "applied", ![i].held = FALSE]  \* it lets go
       IN IF Mutation = "wait_notif"                  \* the planted bug: the side counts now
          THEN Deliver(i, o)
          ELSE /\ ops' = o
               /\ bufst' = SettleAll(lent, o, rx)     \* back or freed if it was the last
               /\ UNCHANGED <<sideVars, rx>>
    /\ UNCHANGED <<task, tread, wire, kfree, peerVars, broken, ringGone>>

\* The shard's ring goes (`ShardRing::shut_down` after its bounded wait): its thread applies no
\* completion again, and what its sends still hold is kept for good (`mem::forget`).
\* MUTATION teardown_frees: the sends still out let go of their buffers as the ring goes.
RingGo ==
    /\ Teardown                                       \* this run lets the ring go
    /\ ~ringGone                                      \* it is still there
    /\ ringGone' = TRUE                               \* and goes
    /\ IF Mutation = "teardown_frees"                 \* the planted bug
       THEN LET o == [i \in DOMAIN ops |-> [ops[i] EXCEPT !.held = FALSE]]  \* all let go
            IN /\ ops' = o
               /\ bufst' = SettleAll(lent, o, rx)     \* so a buffer may be freed now
       ELSE UNCHANGED <<ops, bufst>>                  \* kept: nothing is freed
    /\ UNCHANGED <<task, rx, tread, sideVars, wire, kfree, peerVars, broken>>

(***************************************************************************)
(* The kernel: it takes units, completes, and notifies.                    *)
(***************************************************************************)

\* The kernel takes the next unit of send i into the socket, which has room for it.
Take1(i) ==
    /\ i \in DOMAIN ops                               \* send i exists
    /\ ops[i].first = "no"                            \* and has not completed
    /\ Len(ops[i].took) < Want(ops[i])                \* with units left to take
    /\ ~broken                                        \* the socket works
    /\ Len(wire) - got < Room                         \* and has room for one more
    /\ LET u == ops[i].from + Len(ops[i].took)        \* the send's next unit
       IN /\ wire' = Append(wire, u)                  \* goes on the stream
          /\ ops' = [ops EXCEPT ![i].took = Append(@, u)]  \* and the send took it
    /\ UNCHANGED <<task, rx, tread, sideVars, bufst, kfree, peerVars, broken, ringGone>>

\* The kernel posts the first completion of send i: with the units it took (a short count if
\* fewer than asked), or with none once a cancel came. `IORING_CQE_F_MORE` says a notification
\* follows.
Complete(i) ==
    /\ i \in DOMAIN ops                               \* send i exists
    /\ ops[i].first = "no"                            \* and has not completed
    /\ Len(ops[i].took) > 0 \/ ops[i].cancel          \* it took some, or was cancelled
    /\ ops' = [ops EXCEPT ![i].first = "posted"]      \* its first completion is posted
    /\ UNCHANGED <<task, rx, tread, sideVars, bufst, wire, kfree, peerVars, broken, ringGone>>

\* A send that has taken nothing fails (a reset peer): its first completion carries the error,
\* and the socket takes nothing more.
Fail(i) ==
    /\ MayFail \/ broken                              \* this run lets a send fail, or one did
    /\ i \in DOMAIN ops                               \* send i exists
    /\ ops[i].first = "no"                            \* and has not completed
    /\ ops[i].took = <<>>                             \* and took nothing
    /\ ops' = [ops EXCEPT ![i].first = "posted", ![i].failed = TRUE]  \* it fails
    /\ broken' = TRUE                                 \* and the socket is broken for good
    /\ UNCHANGED <<task, rx, tread, sideVars, bufst, wire, kfree, peerVars, ringGone>>

\* The kernel posts the notification of send i once it let go of every unit the send took
\* (a failed or cancelled send gets one too, as soon as it completed).
Notify(i) ==
    /\ i \in DOMAIN ops                               \* send i exists
    /\ ops[i].first # "no"                            \* it completed
    /\ ops[i].notif = "no"                            \* and has no notification yet
    /\ Range(ops[i].took) \subseteq kfree             \* the kernel let go of its units
    /\ ops' = [ops EXCEPT ![i].notif = "posted"]      \* so the notification is posted
    /\ UNCHANGED <<task, rx, tread, sideVars, bufst, wire, kfree, peerVars, broken, ringGone>>

\* The network acknowledges a unit before the peer read it, so the kernel lets go of it early.
\* Never forced: in the worst case (loopback) a unit is let go of only once the peer read it.
Ack ==
    /\ \E k \in (got + 1)..Len(wire) :                \* a unit the peer has not read
          /\ wire[k] \notin kfree                     \* that the kernel still holds
          /\ kfree' = kfree \cup {wire[k]}            \* is let go of
    /\ UNCHANGED <<task, rx, tread, sideVars, bufst, ops, wire, peerVars, broken, ringGone>>

(***************************************************************************)
(* The peer: it writes first, then reads.                                  *)
(***************************************************************************)

\* The peer writes a unit to the task, if the task's end of the socket has room.
PeerWrite ==
    /\ pwrote < PeerLen                               \* it has more to write
    /\ back < Room2                                   \* and there is room for it
    /\ pwrote' = pwrote + 1                           \* it wrote one more
    /\ back' = back + 1                               \* which waits for the task
    /\ UNCHANGED <<task, rx, tread, sideVars, bufst, ops, wire, kfree, got, broken, ringGone>>

\* The peer reads the next unit of the stream, once its own write is done; the kernel lets go
\* of the unit then, if it still held it.
PeerRead ==
    /\ pwrote = PeerLen                               \* its write is done
    /\ got < Len(wire)                                \* and a unit waits on the stream
    /\ got' = got + 1                                 \* it reads that unit
    /\ kfree' = kfree \cup {wire[got + 1]}            \* and the kernel is done with it
    /\ UNCHANGED <<task, rx, tread, sideVars, bufst, ops, wire, pwrote, back, broken, ringGone>>

(***************************************************************************)
(* The whole system.                                                       *)
(***************************************************************************)

\* Everything ended: the task finished or went, the side writes nothing, the peer read the whole
\* stream, and no buffer is still shared.
Settled ==
    /\ task \in {"done", "gone"}                      \* the task finished or went away
    /\ cur = "none"                                   \* the side writes nothing
    /\ got = Len(wire)                                \* the peer read everything
    /\ pwrote = PeerLen                               \* and wrote everything
    /\ \A b \in Bufs : bufst[b] # "lent"              \* every buffer is back, freed, or unused

\* Once everything ended, nothing more happens.
Rest == Settled /\ UNCHANGED vars

Next ==
    \/ Lend("A", "lendA", "waitA")                    \* the task lends A
    \/ Take("A", "waitA", "lendB")                    \* A's write ended for the task
    \/ Leave                                          \* the task goes away
    \/ Lend("B", "lendB", "waitB")                    \* the task lends B, A maybe still pinned
    \/ Take("B", "waitB", AfterB)                     \* B's write ended for the task
    \/ Read                                           \* the task reads the peer's units
    \/ GetBack("A", "relA", "relB")                   \* A comes back
    \/ Drop("A", "relA", "relB")                      \* or A's Released is dropped
    \/ GetBack("B", "relB", "done")                   \* B comes back
    \/ Drop("B", "relB", "done")                      \* or B's Released is dropped
    \/ Submit                                         \* the side sends a chunk
    \/ \E i \in 1..MaxOps : First(i) \/ Notif(i)      \* the ring takes a completion
    \/ RingGo                                         \* the ring goes
    \/ \E i \in 1..MaxOps : Take1(i) \/ Complete(i) \/ Fail(i) \/ Notify(i)  \* the kernel acts
    \/ Ack                                            \* the network acknowledges a unit
    \/ PeerWrite                                      \* the peer writes
    \/ PeerRead                                       \* the peer reads
    \/ Rest                                           \* everything ended

\* Every step that must happen does, if it stays possible. Leaving, dropping a Released, a
\* failure, the ring going and an early acknowledgment are choices that may never be made.
Fairness ==
    /\ WF_vars(Lend("A", "lendA", "waitA"))           \* the task lends A
    /\ WF_vars(Take("A", "waitA", "lendB"))           \* takes A's end
    /\ WF_vars(Lend("B", "lendB", "waitB"))           \* lends B
    /\ WF_vars(Take("B", "waitB", AfterB))            \* takes B's end
    /\ WF_vars(Read)                                  \* reads
    /\ WF_vars(GetBack("A", "relA", "relB"))          \* gets A back
    /\ WF_vars(GetBack("B", "relB", "done"))          \* gets B back
    /\ WF_vars(Submit)                                \* the side sends
    /\ \A i \in 1..MaxOps : WF_vars(First(i)) /\ WF_vars(Notif(i))      \* the ring applies
    /\ \A i \in 1..MaxOps : WF_vars(Take1(i)) /\ WF_vars(Complete(i)) /\ WF_vars(Notify(i))
                                                      \* the kernel takes, completes, notifies
    /\ \A i \in 1..MaxOps : WF_vars(broken /\ Fail(i))   \* on a broken socket every send fails
    /\ WF_vars(PeerWrite)                             \* the peer writes
    /\ WF_vars(PeerRead)                              \* the peer reads

Spec == Init /\ [][Next]_vars /\ Fairness             \* the behaviours checked

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The stream carries the units in order, each once, and a withdrawn write not at all.
WireOrder == IsPrefix(wire, Whole) /\ (withdrawn => wire = <<>>)

\* Once everything ended, the stream holds both buffers whole if the task stayed (or as far as
\* the socket worked), and A whole or not at all if it went.
Delivered ==
    Settled =>
        /\ task = "done" /\ ~broken => wire = Whole   \* the task stayed and nothing failed
        /\ task = "gone" /\ ~broken => wire \in {<<>>, <<1, 2, 3>>}  \* it went

\* A buffer is handed back or freed only once the kernel can no longer read it.
NoScribble == \A b \in Bufs : bufst[b] \in {"handed", "freed"} => ~KernelReads(b)

\* Everything ends: both writes end without the peer reading first, the task reads, and each
\* buffer comes back (or is freed) after its last notification.
Finishes == <>Settled

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* B goes out while A is still pinned: the writes pipeline.
NoPipeline == (\E i \in DOMAIN ops : ops[i].buf = "B") => bufst["A"] # "lent"
\* A send takes fewer units than it asked for, and the next one goes on from there.
NoShort == \A i \in DOMAIN ops : ~(ops[i].first # "no" /\ 0 < Len(ops[i].took)
                                   /\ Len(ops[i].took) < Want(ops[i]))
\* A buffer whose Released was dropped is freed (at its last notification).
NoLateFree == \A b \in Bufs : bufst[b] # "freed"
\* A write none of which went out is withdrawn.
NoWithdraw == ~withdrawn
\* The side finishes the write of a task that went away.
NoOrphanDone == ~(orphan /\ ~withdrawn /\ cur = "none")
\* The ring goes while a send still waits for its notification: its buffer is kept.
NoKept == ~(ringGone /\ \E i \in DOMAIN ops : ops[i].held /\ ops[i].first = "applied")
\* A send fails, and its buffer still waits for its notification.
NoFailed == ~(\E i \in DOMAIN ops : ops[i].failed /\ ops[i].first = "applied" /\ ops[i].held)
=============================================================================
