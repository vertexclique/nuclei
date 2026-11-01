------------------------------ MODULE Counted ------------------------------
(***************************************************************************)
(* A queue with a count of what it holds (src/rt/shard/counted.rs:         *)
(* `Counted::push`, `is_empty`, `take_all`, and `LazyCounted`'s, which     *)
(* make the queue at its first push), in its two uses: the chunks          *)
(* of a receive group given back on other threads (src/rt/shard/recv.rs:  *)
(* `RecvGroup::take_back`, `has_returns`; src/rt/shard/ring.rs:            *)
(* `ShardRing::take_back`), the registration changes and timer work other *)
(* threads post a shard (src/rt/shard.rs: `Shared::post_io`, `post_timer`, *)
(* `drain_io_ops`, `drain_timer_ops`, `Shard::apply_io_ops`,               *)
(* `has_posted_ops`; src/rt/shard/timers.rs: `fire_timers`;                *)
(* src/rt/shard/ring/keeper.rs: `apply_mailbox`), and the arrivals a ring's *)
(* thread leaves for the holder of a source's read side                    *)
(* (src/io/source/ring.rs: `Source::ring_arrival`; src/io/source/hooks.rs: *)
(* the deferred ARRIVALS).                                                 *)
(*                                                                          *)
(* Who acts. Producers push items. A push claims the queue's next cell,    *)
(* writes its item there, and only then counts it. One consumer at a time *)
(* takes items: it reads the count, pops at most that many from the head, *)
(* stops at a cell whose push is still writing it, and takes off the count *)
(* what it popped. A consumer that reads a count of zero skips the queue:  *)
(* that is what the count is for, a look at one word.                      *)
(*                                                                          *)
(* The returns' use (Ring = FALSE): any thread gives chunks back, and the  *)
(* ring's thread alone takes them in, before each of its waits. A shard's  *)
(* posted work is the same: any thread posts, and the shard applies it;    *)
(* once the shard stopped (or went), the threads that post drain it too,  *)
(* at once with each other (the Drainers): then two passes may read the    *)
(* same count, and the count may go below zero for a moment, until the    *)
(* push whose item a pass took before it was counted counts it. The       *)
(* arrivals' use (Ring = TRUE): the ring's thread alone pushes. When it    *)
(* cannot hold the read side it queues the arrival and leaves ARRIVALS for *)
(* the side's holder, who takes the queue before it lets go. When it holds *)
(* the side it takes what the queue holds first, then hands its arrival in *)
(* directly. Its take of the side may fail even with the side free (its   *)
(* fence failed): the arrival is queued, the work is left in the side's   *)
(* word, and the side's owner is asked to come and do it.                 *)
(*                                                                          *)
(* What must never happen. An item is taken twice, or before an item its  *)
(* producer pushed earlier. With one consumer, a pop made on the count    *)
(* finds the queue empty (the count said more than the queue held). An    *)
(* item stays in the queue for good.                                       *)
(*                                                                          *)
(* What is abstract. The queue is a sequence of cells, each claimed, then *)
(* written, by its push: kovan's SegQueue, whose pop of a claimed cell not *)
(* written yet answers Busy. In the returns' and the arrivals' uses the    *)
(* queue is made at its first push (`LazyCounted`): a look or a take       *)
(* before it finds no queue, which is the empty sequence the model starts  *)
(* from, and the push that makes it publishes it before it claims a cell,  *)
(* so a look that finds the queue finds every cell a push claimed in it.   *)
(* The side's guard is one variable naming its                            *)
(* holder; a holder's release that finds work left does it first, in one *)
(* step with the release, as the guard's compare-and-swap does. Steps are *)
(* sequentially consistent: the count's Release and Acquire are what make *)
(* a counted push's cell written for the consumer that read the count.    *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC                                 \* numbers, sequences, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Producers,  \* the threads that give chunks back (the returns' use)
    Items,      \* the items each producer pushes
    Holds,      \* the times the other thread holds the read side (the arrivals' use)
    Drainers,   \* the threads that drain the queue at once with its consumer (the returns' use)
    Ring,       \* TRUE: the arrivals' use; FALSE: the returns' use
    Mutation    \* "none", or the name of the one rule this run takes away

RingT == "ring"                                                              \* the ring's thread
OtherT == "other"                                                            \* another thread that holds the read side
Total == IF Ring THEN Items ELSE Cardinality(Producers) * Items              \* every item pushed in the end

(* --algorithm Counted
variables                                                                    \* the shared state:
    queue = <<>>,             \* the cells, oldest first, each [p, i, written]
    count = 0,                \* the count: items pushed whole, not taken off yet
    taken = <<>>,             \* every item taken, in the order it was taken, as [p, i]
    guard = "free",           \* who holds the read side (the arrivals' use)
    left = FALSE,             \* ARRIVALS left in the side's word for its holder
    nudged = FALSE,           \* the side's owner was asked to take the side
    ringDone = FALSE,         \* the ring's thread handled its last arrival
    pastEnd = FALSE,          \* (record) a pop made on the count found the queue empty
    busy = FALSE,             \* (record) a pass stopped at a cell still being written
    queuedFirst = FALSE;      \* (record) a direct arrival found queued ones to take first

define                                                                       \* the properties:
    \* No item is taken twice, and none before an item its producer pushed earlier.
    InOrder == \A k, l \in DOMAIN taken :                                    \* for any two takes,
                  (k < l /\ taken[k].p = taken[l].p) => taken[k].i < taken[l].i   \* one producer's come in its order
    \* A pop made on the count always finds a cell: the count never said more than the queue held.
    NoPastEnd == ~pastEnd                                                    \* the record never set
    \* Witness: a pass stops at a push still writing its cell, and the rest waits.
    NoBusy == ~busy                                                          \* claimed never to happen
    \* Witness: a direct arrival finds queued ones, which go in first.
    NoQueuedFirst == ~queuedFirst                                            \* claimed never to happen
    \* Witness: with drainers, the count goes below zero for a moment.
    NeverBelowZero == count >= 0                                             \* claimed never to happen
end define;

\* One consumer's pass (`Counted::take_all`): the count read once, at most that many popped.
procedure TakeAll()
variables want = 0, got = 0;                                                 \* the count read; the items popped
begin                                                                        \* the pass:
T_read:                                                                      \* the count, with Acquire
    want := IF count < 0 THEN Total ELSE count;                              \* read once (below zero, a word that wrapped)
T_pop:                                                                       \* each pop
    while got < want do                                                      \* at most that many
        if queue = <<>> then                                                 \* no cell at all:
            pastEnd := TRUE;                                                 \* the count passed the queue
            goto T_sub;                                                      \* (the pop answers Empty; the pass ends)
        elsif ~Head(queue).written then                                      \* the head's push still writing:
            busy := TRUE;                                                    \* the pop answers Busy,
            goto T_sub;                                                      \* and the rest waits for the next pass
        else                                                                 \* a cell written:
            taken := Append(taken, [p |-> Head(queue).p, i |-> Head(queue).i]);   \* its item is taken
            queue := Tail(queue);                                            \* off the queue
            got := got + 1;                                                  \* one more popped
        end if;                                                              \* one pop
    end while;                                                               \* the pops
T_sub:                                                                       \* off the count
    \* MUTATION sub_read: the pass takes off the count it read, not what it popped.
    count := count - (IF Mutation = "sub_read" THEN want ELSE got);          \* what was popped
    return;                                                                  \* the pass is over
end procedure;

\* A thread that gives chunks back (`Counted::push` from a chunk's drop on another thread).
fair process producer \in (IF Ring THEN {} ELSE Producers)                    \* the returns' use only
variables i = 0;                                                             \* its items pushed
begin                                                                        \* its steps:
P_loop:                                                                      \* the next item
    while i < Items do                                                       \* while it has one
        i := i + 1;                                                          \* this one
P_early:                                                                     \* (mutated) a count before the cell
        \* MUTATION count_first: the push counts its item before it claims a cell.
        if Mutation = "count_first" then count := count + 1; end if;          \* (mutated) the count first
P_claim:                                                                     \* the push claims a cell
        queue := Append(queue, [p |-> self, i |-> i, written |-> FALSE]);    \* the next cell, not written yet
P_write:                                                                     \* the push writes it
        queue := [k \in DOMAIN queue |->                                     \* the queue, with
                    IF queue[k].p = self /\ queue[k].i = i                   \* this push's cell
                    THEN [queue[k] EXCEPT !.written = TRUE]                  \* written
                    ELSE queue[k]];                                          \* and the rest as they were
P_count:                                                                     \* then counts it, with Release
        if Mutation # "count_first" then count := count + 1; end if;          \* once the push returned
    end while;                                                               \* every item pushed
end process;                                                                 \* end of a producer

\* The ring's thread in the returns' use: before each wait it takes in what came back.
fair process taker \in (IF Ring THEN {} ELSE {RingT})                         \* the returns' use only
begin                                                                        \* its steps:
K_loop:                                                                      \* the next wait
    while Len(taken) < Total do                                              \* while chunks are out
K_look:                                                                      \* `has_returns`: one look at the count
        if count # 0 then                                                    \* chunks counted:
            call TakeAll();                                                  \* taken in
        end if;                                                              \* else the queue is skipped
    end while;                                                               \* every chunk in
end process;                                                                 \* end of the taker

\* A thread that drains the queue at once with its consumer (a poster to a shard that stopped).
fair process drainer \in (IF Ring THEN {} ELSE Drainers)                       \* the returns' use only
begin                                                                        \* its steps:
D_loop:                                                                      \* the next drain
    while Len(taken) < Total do                                              \* while items are out
D_look:                                                                      \* one look at the count
        if count # 0 then                                                    \* items counted:
            call TakeAll();                                                  \* taken
        end if;                                                              \* else the queue is skipped
    end while;                                                               \* every item taken
end process;                                                                 \* end of a drainer

\* The ring's thread in the arrivals' use: each arrival goes in directly, or through the queue.
fair process ring \in (IF Ring THEN {RingT} ELSE {})                          \* the arrivals' use only
variables j = 0;                                                             \* its arrivals so far
begin                                                                        \* its steps:
R_loop:                                                                      \* the next arrival
    while j < Items do                                                       \* while one comes
        j := j + 1;                                                          \* this one
R_arrive:                                                                    \* `try_hold_for`
        either                                                               \* the side taken:
            await guard = "free";                                            \* it was free
            guard := RingT;                                                  \* and is the ring's thread's now
R_look:                                                                      \* `is_empty`: what waits came first
            \* MUTATION no_look: the arrival goes in with no look at the queue.
            if count # 0 /\ Mutation # "no_look" then                         \* arrivals counted:
                queuedFirst := TRUE;                                         \* (record) a queued one goes first
                call TakeAll();                                              \* taken in, in order
            end if;                                                          \* the queue is done
R_direct:                                                                    \* `take_arrival`
            taken := Append(taken, [p |-> RingT, i |-> j]);                  \* this arrival, after them
            goto R_release;                                                  \* then the side is let go
        or                                                                   \* the side not taken (held, or a fence that failed):
            skip;                                                            \* the arrival is queued
        end either;                                                          \* the take is decided
R_claim:                                                                     \* the push claims a cell
        queue := Append(queue, [p |-> RingT, i |-> j, written |-> FALSE]);   \* the next cell, not written yet
R_write:                                                                     \* writes it
        queue := [k \in DOMAIN queue |->                                     \* the queue, with
                    IF queue[k].i = j THEN [queue[k] EXCEPT !.written = TRUE]   \* this cell written
                    ELSE queue[k]];                                          \* and the rest as they were
R_count:                                                                     \* counts it
        \* MUTATION count_late: the arrival is counted only after ARRIVALS is left.
        if Mutation # "count_late" then count := count + 1; end if;           \* once the push returned
R_defer:                                                                     \* `defer(ARRIVALS)`
        either                                                               \* a take through the word:
            if guard = "free" then                                           \* the side free:
                guard := RingT;                                              \* held,
                left := TRUE;                                                \* the work its own to do
            else                                                             \* held by another:
                left := TRUE;                                                \* the work left for the holder
            end if;                                                          \* the work is placed
        or                                                                   \* a foreign take whose fence failed:
            left := TRUE;                                                    \* the work stays in the word
            nudged := TRUE;                                                  \* and the owner is asked to come
        end either;                                                          \* the defer is placed
R_late:                                                                      \* (mutated) the count after the defer
        if Mutation = "count_late" then count := count + 1; end if;           \* (mutated)
        if guard # RingT then goto R_loop; end if;                           \* not holding: the next arrival
R_release:                                                                   \* `release`: what was left, then free
        if left then                                                         \* work left in the word:
            left := FALSE;                                                   \* taken with the flags,
            call TakeAll();                                                  \* done (the arrivals queued),
            goto R_release;                                                  \* and the release tries again
        else                                                                 \* nothing left:
            guard := "free";                                                 \* the side goes free
        end if;                                                              \* let go
    end while;                                                               \* every arrival handled
R_done:                                                                      \* nothing more comes
    ringDone := TRUE;                                                        \* from the ring's thread
end process;                                                                 \* end of the ring's thread

\* Another thread that holds the read side now and then, the owner a nudge asks to come.
fair process other \in (IF Ring THEN {OtherT} ELSE {})                        \* the arrivals' use only
variables h = 0;                                                             \* its holds so far
begin                                                                        \* its steps:
O_loop:                                                                      \* the next hold
    while h < Holds \/ nudged \/ ~ringDone do                                \* while it polls, or a nudge may come
O_when:                                                                      \* why it takes the side
        await h < Holds \/ nudged \/ ringDone;                                \* a poll of its own, a nudge, or the end
        if h < Holds then                                                    \* a poll of its own:
            h := h + 1;                                                      \* one more hold
        elsif ~nudged then                                                   \* no poll and no nudge:
            goto O_loop;                                                     \* the ring's thread is done
        end if;                                                              \* a nudge: the owner comes
O_take:                                                                      \* takes the side
        await guard = "free";                                                \* once free
        guard := OtherT;                                                     \* held,
        nudged := FALSE;                                                     \* the nudge answered
O_release:                                                                   \* `release`: what was left, then free
        if left then                                                         \* work left in the word:
            left := FALSE;                                                   \* taken with the flags,
            call TakeAll();                                                  \* done (the arrivals queued),
            goto O_release;                                                  \* and the release tries again
        else                                                                 \* nothing left:
            guard := "free";                                                 \* the side goes free
        end if;                                                              \* let go
    end while;                                                               \* no more holds
end process;                                                                 \* end of the other thread
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The property over whole runs.                                           *)
(***************************************************************************)

\* Every item pushed is taken in the end: none stays in the queue for good.
AllTaken == <>(Len(taken) = Total)                                           \* every item, eventually
=============================================================================
