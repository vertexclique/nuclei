---------------------------- MODULE PoolIndex ----------------------------
(***************************************************************************)
(* The blocking pool with an index of its idle slots (change F2 of the     *)
(* network-path work, not written yet): src/rt/blocking.rs, `Pool::submit` *)
(* and `Pool::wake_one` for the submitter, `Pool::work` for a thread. Today *)
(* a submit scans every slot from a rotating hint with a compare-and-swap  *)
(* (idle to woken) per slot. The plan adds a bitmap beside the slots, one  *)
(* bit per slot: a submitter finds a candidate with loads of the bitmap    *)
(* and tries the compare-and-swap on that slot only, which stays the one   *)
(* step that claims a thread.                                              *)
(*                                                                          *)
(* Who acts. Submitters push jobs and wake or start threads. The pool's    *)
(* threads take jobs, go idle in a slot of their own, and may leave at     *)
(* their keep-alive.                                                       *)
(*                                                                          *)
(* What can happen. A submitter pushes its job; if the idle count is zero  *)
(* it starts a thread while the pool has room. Else it reads the bitmap    *)
(* one slot at a time from a hint: a set bit is a candidate, and it tries  *)
(* that slot's compare-and-swap (idle to woken); won, it counts the thread *)
(* out and wakes it; lost (the bit was stale), it goes on with the next    *)
(* candidate. With no candidate left it falls back: it starts a thread     *)
(* while there is room (or, in the configurations whose bitmap loads may   *)
(* read stale values, first runs today's full scan). A thread with nothing *)
(* to take counts itself idle, marks its slot idle, sets its bit, and only *)
(* then looks at the queue once more: a job there, it takes its slot back. *)
(* Only a slot's own thread writes its bit: it clears it whenever its slot *)
(* leaves idle, after taking it back at its look, and before its slot's   *)
(* compare-and-swap at the end of its wait (woken, or freed at its        *)
(* keep-alive), since a freed slot may be another thread's at once. Every *)
(* other access is sequentially consistent, as today.                      *)
(*                                                                          *)
(* What must never happen. A job is pushed and never runs. Two threads run *)
(* one job. The pool counts more threads than its most. The idle count    *)
(* goes below zero.                                                        *)
(*                                                                          *)
(* What is abstract, as in Pool.tla: the queue is a count (one taker at a  *)
(* time, as the inbox's role makes it), a thread's park is its wait for   *)
(* its slot, a start hands a token to a thread not running, and today's   *)
(* full scan is one step that claims any idle slot. The bitmap is one     *)
(* word (the configurations have at most three slots); a scan over more   *)
(* words reads each word with its own load, and a bit is missed or seen   *)
(* by its own word's load alone, which a read of one slot's bit per step   *)
(* covers. A load of the bitmap is sequentially consistent (`IndexLoad =   *)
(* "seqcst"`, a plain load on x86) or may return any value for a bit      *)
(* (`IndexLoad = "relaxed"`, a stale relaxed load on a weaker machine).    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC                                            \* numbers, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Submitters, \* the submitting threads (numbered past the pool's), each sending `Jobs` jobs
    Jobs,       \* the jobs each submitter sends
    Max,        \* the pool's most threads, and its slots
    Timeouts,   \* whether an idle thread's keep-alive can run out
    IndexLoad,  \* "seqcst", or "relaxed" when a load of the bitmap may read any value
    Fallback,   \* "start" (start a thread) or "scan" (today's full scan, then a start) when no candidate took
    Mutation    \* "none", or the name of the one rule this run takes away

Threads == 1..Max                                                            \* the pool's threads
Slots == 1..Max                                                              \* its slots, one per thread at most
Total == Cardinality(Submitters) * Jobs                                      \* every job sent
NoSlot == 0                                                                  \* no slot
After(k) == (k % Max) + 1                                                    \* the slot after `k`, round the slots

(* --algorithm PoolIndex                                                       \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    queue = 0,                         \* jobs pushed and not taken
    ran = 0,                           \* jobs run
    threads = 0,                       \* the pool's count of its threads
    starts = 0,                        \* start tokens not taken by a thread yet
    slot = [i \in Slots |-> "free"],   \* each slot: free, busy, idle, woken
    idle = 0,                          \* the count of idle threads
    index = {},                        \* the bitmap: the slots whose bit is set
    me = [t \in Threads |-> NoSlot],   \* each thread's slot
    woke = FALSE,                      \* (record) a submitter woke an idle thread
    staleCas = FALSE,                  \* (record) a set bit sent a submitter to a slot no longer idle
    staleStart = FALSE,                \* (record) a submitter that saw threads idle found no candidate and started a thread
    lookFound = FALSE,                 \* (record) a thread's look after going idle found a job
    timedOut = FALSE,                  \* (record) a thread's keep-alive ran out and it left
    reclaimed = FALSE;                 \* (record) a leaving thread took a job back

define                                                                       \* names the code and the properties use:
    \* The threads that hold slot `i`.
    Owners(i) == {t \in Threads : me[t] = i}                                 \* one at most
end define;                                                                  \* end of the names

\* A submitter (`Pool::submit`, `Pool::wake_one`).
fair process submitter \in Submitters                                        \* one process per submitter, weakly fair
variables                                                                    \* each submitter's own state:
    sent = 0,                                                                \* the jobs it sent
    k = NoSlot,                                                              \* the slot its scan of the bitmap is at
    scanned = 0,                                                             \* the slots that scan has passed
    seen = FALSE,                                                            \* the bit it read
    claimed = NoSlot,                                                        \* the slot it claimed
    missed = FALSE;                                                          \* it saw threads idle and no candidate took
begin                                                                        \* a submitter's steps:
S_loop:                                                                      \* its next job
    while sent < Jobs do                                                     \* while it has jobs to send
S_push:                                                                      \* the push
        queue := queue + 1;                                                  \* the job is in the queue
        sent := sent + 1;                                                    \* and counted as sent
        missed := FALSE;                                                     \* a new submit
S_look:                                                                      \* the look at the idle count
        if idle = 0 then goto S_start; end if;                               \* no thread idle: start one
S_hint:                                                                      \* the scan's start
        with h \in Slots do k := h; end with;                                \* the rotating hint: any slot
        scanned := 0;                                                        \* nothing passed yet
S_read:                                                                      \* a load of one slot's bit
        if scanned = Max then goto S_none;                                   \* every slot passed: no candidate took
        else                                                                 \* slot `k`'s bit:
            with b \in IF IndexLoad = "relaxed" THEN BOOLEAN                 \* a stale load may read anything,
                        ELSE IF k \in index THEN {TRUE} ELSE {FALSE} do      \* a sequentially consistent one reads the bit
                seen := b;                                                   \* the bit it read
            end with;                                                        \* one load
            if seen then goto S_cas;                                         \* set: a candidate
            else k := After(k); scanned := scanned + 1; goto S_read;          \* clear: the next slot
            end if;                                                          \* the scan goes on
        end if;                                                              \* or it is over
S_cas:                                                                       \* the candidate's claim
        \* MUTATION trust_index: the submitter takes the candidate for claimed and stores woken without the compare-and-swap.
        if Mutation = "trust_index" then                                     \* (mutated) a plain store:
            slot[k] := "woken";                                              \* the slot is marked woken whatever it was,
            claimed := k;                                                    \* claimed,
            woke := TRUE;                                                    \* (record)
            goto S_uncount;                                                  \* and counted out
        elsif slot[k] = "idle" then                                          \* the compare-and-swap, idle to woken, wins:
            slot[k] := "woken";                                              \* the thread is claimed,
            claimed := k;                                                    \* by this submitter,
            woke := TRUE;                                                    \* (record)
            goto S_uncount;                                                  \* and counted out
        else                                                                 \* it loses: the bit was stale
            staleCas := TRUE;                                                \* (record)
            k := After(k);                                                    \* the next slot,
            scanned := scanned + 1;                                          \* this one passed,
            goto S_read;                                                     \* and the scan goes on
        end if;                                                              \* the candidate is done with
S_uncount:                                                                   \* the claimed thread counted out
        idle := idle - 1;                                                    \* one idle thread fewer, and it is woken
S_clear:                                                                     \* (mutated) the submitter clears the bit
        \* MUTATION submitter_clears: the submitter clears the claimed slot's bit, and the woken thread does not.
        if Mutation = "submitter_clears" then                                \* (mutated) after its claim:
            index := index \ {claimed};                                      \* the bit is cleared, whatever the thread did since
        end if;                                                              \* else the thread clears it as it wakes
        claimed := NoSlot;                                                   \* the claim is spent
        goto S_loop;                                                         \* the job has a thread
S_none:                                                                      \* no candidate took
        missed := TRUE;                                                      \* threads looked idle, none was claimed
        \* MUTATION no_fallback: finding no candidate, the submitter neither starts a thread nor rescans.
        if Mutation = "no_fallback" then goto S_loop;                        \* (mutated) it gives up
        elsif Fallback = "scan" then                                         \* today's full scan first:
            with i \in IF \E j \in Slots : slot[j] = "idle"                  \* any slot idle by then,
                       THEN {j \in Slots : slot[j] = "idle"} ELSE {NoSlot} do   \* or none
                if i # NoSlot then                                           \* one is:
                    slot[i] := "woken";                                      \* its compare-and-swap wins,
                    claimed := i;                                            \* it is claimed,
                    woke := TRUE;                                            \* (record)
                end if;                                                      \* else nothing
            end with;                                                        \* one step, as in Pool.tla
            if claimed # NoSlot then goto S_uncount; end if;                 \* claimed: counted out
        end if;                                                              \* else a start
S_start:                                                                     \* a start
        if threads < Max then                                                \* room for a thread:
            threads := threads + 1;                                          \* counted,
            starts := starts + 1;                                            \* and started
            if missed then staleStart := TRUE; end if;                       \* (record)
        end if;                                                              \* else the threads there will see the job
    end while;                                                               \* the next job
end process;                                                                 \* end of a submitter

\* A pool thread (`Pool::work`).
fair process thread \in Threads                                              \* one process per thread, weakly fair
variables lateSlot = NoSlot;                                                 \* (mutated) the slot it freed and clears the bit of after
begin                                                                        \* a thread's steps:
T_dormant:                                                                   \* not running yet
    await starts > 0;                                                        \* a start for it:
    starts := starts - 1;                                                    \* taken
T_slot:                                                                      \* its slot (`claim_slot`)
    with i \in {j \in Slots : slot[j] = "free"} do                           \* a free slot,
        slot[i] := "busy";                                                   \* taken busy,
        me[self] := i;                                                       \* and its own
    end with;                                                                \* one compare-and-swap
T_take:                                                                      \* the jobs
    while queue > 0 do                                                       \* while the queue has one:
        queue := queue - 1;                                                  \* it takes it,
T_run:                                                                       \* the run
        ran := ran + 1;                                                      \* and runs it
    end while;                                                               \* the queue is empty
T_count:                                                                     \* idle: the count
    idle := idle + 1;                                                        \* it counts itself idle
T_idle:                                                                      \* idle: the slot
    slot[me[self]] := "idle";                                                \* store(IDLE)
T_bit:                                                                       \* idle: the bit
    \* MUTATION bit_after_look: the thread sets its bit after its look at the queue, not before.
    if Mutation # "bit_after_look" then                                      \* in order:
        index := index \cup {me[self]};                                      \* its bit is set, before the look
    end if;                                                                  \* (mutated: later)
T_look:                                                                      \* the look at the queue
    if queue > 0 then                                                        \* a job pushed while it went idle:
        lookFound := TRUE;                                                   \* (record)
        if slot[me[self]] = "idle" then                                      \* its compare-and-swap, idle to busy, wins:
            slot[me[self]] := "busy";                                        \* its slot back,
            goto T_look_uncount;                                             \* and it counts itself out
        else                                                                 \* a submitter claimed it first:
            slot[me[self]] := "busy";                                        \* busy, counted out by that submitter
            goto T_claimed_clear;                                            \* then its bit
        end if;                                                              \* either way it takes jobs again
    end if;                                                                  \* no job: it waits
T_late_bit:                                                                  \* (mutated) the bit after the look
    if Mutation = "bit_after_look" then                                      \* (mutated) only now:
        index := index \cup {me[self]};                                      \* its bit is set
    end if;                                                                  \* else nothing here
T_wait:                                                                      \* the wait
    either                                                                   \* until its slot is claimed,
        await slot[me[self]] = "woken";                                      \* woken,
    or                                                                       \* or its keep-alive runs out
        await Timeouts;                                                      \* when it can
    end either;                                                              \* the wait is over
T_after:                                                                     \* after the wait: its bit first
    \* The bit is cleared before the slot leaves idle: a slot freed first may be another thread's at once,
    \* and a late clear would erase that thread's bit.
    \* MUTATION never_clear: a woken slot's bit is never cleared.
    \* MUTATION clear_after_free: at its keep-alive the thread frees its slot first and clears its bit after.
    if (slot[me[self]] = "woken" /\ Mutation \in {"never_clear", "submitter_clears"})   \* (mutated) woken, left set,
       \/ (slot[me[self]] = "idle" /\ Mutation = "clear_after_free") then   \* (mutated) or cleared after the free:
        skip;                                                                \* not now
    else                                                                     \* in order:
        index := index \ {me[self]};                                         \* its bit is cleared
    end if;                                                                  \* then its slot moves
T_move:                                                                      \* its slot leaves idle
    if slot[me[self]] = "idle" then                                          \* its compare-and-swap, idle to free, wins:
        slot[me[self]] := "free";                                            \* the slot is free,
        timedOut := TRUE;                                                    \* (record)
        if Mutation = "clear_after_free" then                                \* (mutated) its bit only now:
            lateSlot := me[self];                                            \* the slot it freed,
            me[self] := NoSlot;                                              \* no longer its,
            goto T_late_clear;                                               \* is cleared next
        else                                                                 \* in order:
            me[self] := NoSlot;                                              \* no longer its
        end if;                                                              \* it leaves
    else                                                                     \* it loses: a submitter claimed the slot,
        slot[me[self]] := "busy";                                            \* busy, counted out by that submitter
        goto T_take;                                                         \* and the jobs
    end if;                                                                  \* it leaves
T_after_uncount:                                                             \* out of the idle count
    idle := idle - 1;                                                        \* counted out
T_leave:                                                                     \* out of the thread count
    threads := threads - 1;                                                  \* counted out
T_last_look:                                                                 \* the last look at the queue
    if queue > 0 /\ threads < Max then                                       \* a job no thread was started for, and room:
        threads := threads + 1;                                              \* counted again,
        reclaimed := TRUE;                                                   \* (record)
        goto T_slot;                                                         \* with a slot again
    end if;                                                                  \* else it is gone
T_rest:                                                                      \* not running
    goto T_dormant;                                                          \* until started again
T_look_uncount:                                                              \* a slot taken back: the count
    idle := idle - 1;                                                        \* it counts itself out
T_back_clear:                                                                \* a slot taken back: the bit
    index := index \ {me[self]};                                             \* cleared
    goto T_take;                                                             \* and the jobs
T_claimed_clear:                                                             \* claimed as it looked: the bit
    \* MUTATION never_clear: a woken slot's bit is never cleared.
    if Mutation \notin {"never_clear", "submitter_clears"} then              \* in order:
        index := index \ {me[self]};                                         \* cleared
    end if;                                                                  \* (mutated: left set, or the submitter's)
    goto T_take;                                                             \* and the jobs
T_late_clear:                                                                \* (mutated) the bit after the free
    index := index \ {lateSlot};                                             \* cleared, the slot maybe another's by now
    lateSlot := NoSlot;                                                      \* done
    goto T_after_uncount;                                                    \* it leaves
end process;                                                                 \* end of a thread
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* A job runs once: never more runs than jobs pushed, so no two threads run one job.
RunOnce == ran + queue <= Total /\ queue >= 0                                \* runs and queued never pass the jobs sent

\* The pool keeps to its most threads.
Bounded == threads <= Max /\ starts >= 0                                     \* the cap holds

\* A slot has at most one thread, and the idle count never goes below zero (it is unsigned).
OneOwner == \A i \in Slots : Cardinality(Owners(i)) <= 1                     \* one thread per slot
IdleCounted == idle >= 0 /\ idle <= Max + Cardinality(Submitters)            \* never below zero, at most one claim each above the idle

\* The bitmap misses no thread that waits idle: a thread past its bit and still idle has its bit set.
IndexComplete ==                                                             \* for every thread:
    \A t \in Threads :                                                       \* one waiting in its idle slot
        (pc[t] \in {"T_look", "T_late_bit", "T_wait"} /\ me[t] # NoSlot      \* past its bit
         /\ slot[me[t]] = "idle" /\ Mutation # "bit_after_look")             \* (the mutation sets it later)
        => me[t] \in index                                                   \* is in the bitmap

\* A set bit names a slot that is idle or woken, or one whose thread is about to clear it.
Clearing == {"T_look_uncount", "T_back_clear", "T_claimed_clear"}         \* a thread between its slot's move and its clear
IndexTight ==                                                                \* for every set bit:
    \A i \in index :                                                         \* its slot
        \/ slot[i] \in {"idle", "woken"}                                     \* is idle or claimed,
        \/ \E t \in Threads : (me[t] = i /\ pc[t] \in Clearing)              \* or its thread is about to clear it,
        \/ \E t \in Threads : (lateSlot[t] = i /\ pc[t] = "T_late_clear")    \* (mutated) or its old thread will

\* Every job pushed runs.
AllRan == <>(ran = Total)                                                    \* in the end, every job ran

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A submitter that saw threads idle finds no candidate in the bitmap and starts a thread.
NoStaleStart == ~staleStart                                                  \* never sent to a start
\* A set bit sends a submitter to a slot no longer idle, and its compare-and-swap loses.
NoStaleCas == ~staleCas                                                      \* never a stale candidate
\* A submitter wakes an idle thread.
NoWake == ~woke                                                              \* never a wake
=============================================================================
