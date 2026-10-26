------------------------------ MODULE Pool ------------------------------
(***************************************************************************)
(* A runtime's blocking pool (src/rt/blocking.rs): the jobs, the threads   *)
(* that take them, and the slots idle threads wait in, at the grain of     *)
(* each atomic access.                                                    *)
(*                                                                          *)
(* In plain words: a submitter pushes its job into the pool's queue (the  *)
(* inbox's, linked through the tasks: checked in `inbox`), then, if the   *)
(* count of idle threads says one waits, claims an idle slot (idle to     *)
(* woken) and unparks its thread; if it claims none, it starts a thread   *)
(* while the pool has room for one. A thread takes jobs until the queue   *)
(* is empty, then counts itself idle, marks its slot idle, and looks at   *)
(* the queue once more: a job there, it takes its slot back (idle to busy)*)
(* unless a submitter claimed it first, and goes on. Else it waits until  *)
(* its slot is claimed or its time is up; at its time it frees its slot   *)
(* (idle to free, unless claimed meanwhile), counts itself out, and looks *)
(* at the queue once more: a job a submitter found no room to start a     *)
(* thread for is its own to take, with a slot and a place in the count    *)
(* taken back. Every access is sequentially consistent: either a thread's *)
(* look sees a job, or the job's submitter sees the thread idle.          *)
(*                                                                          *)
(* What is abstract: the queue is a count (one taker at a time, as the     *)
(* inbox's role makes it), a thread's park is its wait for its slot, and  *)
(* a start hands a token to a thread not running.                         *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Submitters, \* the submitting threads (numbered past the pool's), each sending `Jobs` jobs
    Jobs,
    Max,        \* the pool's most threads
    Timeouts,   \* whether an idle thread's time can run out
    Mutation    \* "none", or the name of the one rule this run takes away

Threads == 1..Max
Slots == 1..Max
Total == Cardinality(Submitters) * Jobs
NoSlot == 0

(* --algorithm Pool
variables
    queue = 0,                         \* jobs pushed and not taken
    ran = 0,                           \* jobs run
    threads = 0,                       \* the pool's count of its threads
    starts = 0,                        \* start tokens not taken by a thread yet
    slot = [i \in Slots |-> "free"],   \* free, busy, idle, woken
    idle = 0,                          \* the count of idle threads
    me = [t \in Threads |-> NoSlot],   \* each thread's slot
    woke = FALSE,                      \* (record) a submitter woke an idle thread
    lookFound = FALSE,                 \* (record) a thread's look after going idle found a job
    timedOut = FALSE,                  \* (record) a thread's time ran out and it left
    reclaimed = FALSE;                 \* (record) a leaving thread took a job back

define
    Owners(i) == {t \in Threads : me[t] = i}
end define;

fair process submitter \in Submitters
variables sent = 0, claimed = NoSlot;
begin
S_loop:
    while sent < Jobs do
S_push:
        queue := queue + 1;
        sent := sent + 1;
S_look:
        if idle > 0 then
S_claim:
            \* The scan's claim of an idle slot, a compare-and-swap; none if
            \* no slot is idle by then.
            with i \in IF \E j \in Slots : slot[j] = "idle"
                       THEN {j \in Slots : slot[j] = "idle"} ELSE {NoSlot} do
                if i # NoSlot then
                    slot[i] := "woken";
                    woke := TRUE;
                    claimed := i;
                end if;
            end with;
S_uncount:
            if claimed # NoSlot then
                idle := idle - 1;
                claimed := NoSlot;
                goto S_loop;
            end if;
        end if;
S_start:
        if threads < Max then
            threads := threads + 1;
            starts := starts + 1;
        end if;
    end while;
end process;

fair process thread \in Threads
begin
T_dormant:
    await starts > 0;
    starts := starts - 1;
T_slot:
    with i \in {j \in Slots : slot[j] = "free"} do
        slot[i] := "busy";
        me[self] := i;
    end with;
T_take:
    while queue > 0 do
        queue := queue - 1;
T_run:
        ran := ran + 1;
    end while;
T_count:
    idle := idle + 1;
T_idle:
    slot[me[self]] := "idle";
T_look:
    \* MUTATION no_look: the thread waits without looking at the queue again.
    if queue > 0 /\ Mutation # "no_look" then
        lookFound := TRUE;
        \* Its slot back, unless a submitter claimed it (and counted it out).
        if slot[me[self]] = "idle" then
            slot[me[self]] := "busy";
T_look_uncount:
            idle := idle - 1;
        else
            slot[me[self]] := "busy";
        end if;
T_look_back:
        goto T_take;
    end if;
T_wait:
    either
        await slot[me[self]] = "woken";
    or
        await Timeouts;
    end either;
T_after:
    if slot[me[self]] = "woken" then
        slot[me[self]] := "busy";
        goto T_take;
    elsif slot[me[self]] = "idle" then
        slot[me[self]] := "free";
        me[self] := NoSlot;
        timedOut := TRUE;
T_after_uncount:
        idle := idle - 1;
    else
        goto T_take;
    end if;
T_leave:
    threads := threads - 1;
T_last_look:
    \* MUTATION no_last_look: a leaving thread does not look at the queue.
    if queue > 0 /\ Mutation # "no_last_look" then
        if threads < Max then
            threads := threads + 1;
            reclaimed := TRUE;
            goto T_slot;
        end if;
    end if;
T_rest:
    goto T_dormant;
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

\* A job runs once: never more runs than jobs pushed.
RunOnce == ran + queue <= Total /\ queue >= 0

\* The pool keeps to its most threads, and its count to the threads alive.
Bounded == threads <= Max /\ starts >= 0

\* A slot has at most one thread. The idle count never goes below zero (it
\* is unsigned); it may pass the idle threads by the claims a submitter has
\* made and not counted out yet, one each at most.
OneOwner == \A i \in Slots : Cardinality(Owners(i)) <= 1
IdleCounted == idle >= 0 /\ idle <= Max + Cardinality(Submitters)

\* Every job pushed runs.
AllRan == <>(ran = Total)

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A submitter wakes an idle thread.
NoWake == ~woke
\* A thread's look after going idle finds a job.
NoLookFound == ~lookFound
\* A thread's time runs out and it leaves.
NoTimeout == ~timedOut
\* A leaving thread takes back a job no thread was started for.
NoReclaim == ~reclaimed
=============================================================================
