------------------------------ MODULE ParkLook ------------------------------
(***************************************************************************)
(* The last look of a shard that parks, at work other threads posted to   *)
(* its mailboxes (src/rt/shard.rs: `has_remote_work`, `has_remote_tasks`, *)
(* `has_posted_ops`; src/rt/shard/counted.rs: `Counted::is_empty_seqcst`; *)
(* src/rt/handshake.rs: `ParkState::begin_park`, `wake`; `Shared::post_io` *)
(* and `unpark` in src/rt/shard.rs), on a machine whose sequentially      *)
(* consistent loads wait for the thread's own sequentially consistent      *)
(* stores but whose plain loads may pass them, as aarch64's do (a `ldr`    *)
(* may be satisfied before an earlier `stlr` is seen; a `ldar` may not).   *)
(*                                                                          *)
(* Who acts. The shard S, which says it parks (a sequentially consistent  *)
(* store of its flag) and then looks once more: at its inbox's head (a     *)
(* sequentially consistent load) and at the count of what was posted to   *)
(* its mailbox. The poster P, another thread, which posts an operation    *)
(* (the count's increment), fences, and then looks whether S parks, and if *)
(* so wakes it.                                                            *)
(*                                                                          *)
(* What must never happen. S sleeps with the operation posted and nobody  *)
(* to wake it: its look read the old count while P's look read S awake.    *)
(*                                                                          *)
(* What is checked. With the count's load sequentially consistent, S sees *)
(* the post or P sees S parked, whichever word S looks at first. With the  *)
(* count's load plain, it holds only while the inbox's sequentially        *)
(* consistent load comes first (which held the plain load back): looked at *)
(* first, the plain load passes S's store and the wake is lost.            *)
(*                                                                          *)
(* What is abstract. One post; the inbox holds nothing (its push is        *)
(* Handshake.tla's); the notification is a counter S waits on; a store     *)
(* buffer per thread stands for every way a store is seen late.            *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, buffers, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    CountLoad,  \* the count's load: "seqcst" (the fix) or "plain" (relaxed, as before it)
    Order       \* what S looks at first: "inbox" (the code's order) or "count"

S == 1                                                                       \* the shard
P == 2                                                                       \* the poster
Flusher == 0                                                                 \* the memory system
Threads == {S, P}                                                            \* the two threads
Words == {"parked", "count", "head"}                                         \* the words they share

\* What thread `t` reads of word `v`: its own newest buffered store, else memory.
Newest(b, v) == {i \in DOMAIN b : b[i][1] = v /\ \A j \in DOMAIN b : b[j][1] = v => j <= i}  \* its newest store of v
Read(mem, buf, t, v) ==                                                      \* the value read
    IF Newest(buf[t], v) = {} THEN mem[v] ELSE buf[t][CHOOSE i \in Newest(buf[t], v) : TRUE][2]  \* forwarded, else memory

(* --algorithm ParkLook                                                      \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    mem = [v \in Words |-> 0],      \* the memory: nobody parked, nothing posted, the inbox empty
    buf = [t \in Threads |-> <<>>], \* each thread's stores not yet seen by the other
    notified = 0;                   \* wakes S got

define                                                                       \* names the properties use:
    \* Everything ends: S found the post, or slept and was woken; P is done.
    AllEnd == <>(pc[S] = "Done" /\ pc[P] = "Done")                           \* in the end
end define;                                                                  \* end of the names

\* The memory system: a buffer's oldest store reaches memory, at any time.
fair process flusher = Flusher                                               \* weakly fair: stores are seen in the end
begin                                                                        \* its one step, again and again:
Flush:                                                                       \* one store reaches memory
    while TRUE do                                                            \* forever
        with t \in {u \in Threads : buf[u] # <<>>} do                        \* a thread with a store buffered
            mem[Head(buf[t])[1]] := Head(buf[t])[2];                         \* its oldest store lands
            buf[t] := Tail(buf[t]);                                          \* and leaves the buffer
        end with;                                                            \* one at a time
    end while;                                                               \* the next
end process;                                                                 \* end of the memory system

\* S: `begin_park` and its look (`has_remote_work`).
fair process shard = S                                                       \* weakly fair: S runs
variables first = TRUE, seen = 0;                                            \* which look is next, what it read
begin                                                                        \* S's steps:
S_store:                                                                     \* parked := true (sequentially consistent)
    buf[S] := Append(buf[S], <<"parked", 1>>);                               \* buffered: seen by P later
S_look1:                                                                     \* the first look
    if Order = "inbox" then                                                  \* the inbox's head first
        await buf[S] = <<>>;                                                 \* a sequentially consistent load waits for S's store
        seen := Read(mem, buf, S, "head");                                   \* the head (nothing pushed)
    elsif CountLoad = "seqcst" then                                          \* the count first, sequentially consistent
        await buf[S] = <<>>;                                                 \* waits for S's store
        seen := Read(mem, buf, S, "count");                                  \* the count
    else                                                                     \* the count first, plain
        seen := Read(mem, buf, S, "count");                                  \* may pass S's store
    end if;                                                                  \* one word read
S_found1:                                                                    \* work seen: S backs out
    if seen = 1 then goto Done; end if;                                      \* found
S_look2:                                                                     \* the second look
    if Order = "count" then                                                  \* then the inbox's head
        await buf[S] = <<>>;                                                 \* sequentially consistent
        seen := Read(mem, buf, S, "head");                                   \* the head
    elsif CountLoad = "seqcst" then                                          \* then the count, sequentially consistent
        await buf[S] = <<>>;                                                 \* waits for S's store
        seen := Read(mem, buf, S, "count");                                  \* the count
    else                                                                     \* then the count, plain
        seen := Read(mem, buf, S, "count");                                  \* after a load that waited for the store
    end if;                                                                  \* both words read
S_found2:                                                                    \* work seen: S backs out
    if seen = 1 then goto Done; end if;                                      \* found
S_sleep:                                                                     \* asleep until woken
    await notified > 0;                                                      \* P's wake
end process;                                                                 \* end of S

\* P: `post_io`, then `unpark`.
fair process poster = P                                                      \* weakly fair: P runs
variables saw = 0;                                                           \* what its look read of S's flag
begin                                                                        \* P's steps:
P_post:                                                                      \* the push and the count's increment (release)
    buf[P] := Append(buf[P], <<"count", 1>>);                                \* buffered
P_fence:                                                                     \* fence(SeqCst): the increment is seen first
    await buf[P] = <<>>;                                                     \* drained
P_look:                                                                      \* `wake`: whether S parks (sequentially consistent)
    saw := Read(mem, buf, P, "parked");                                      \* S's flag
P_wake:                                                                      \* parked: S is woken
    if saw = 1 then notified := notified + 1; end if;                        \* the notification
end process;                                                                 \* end of P
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
