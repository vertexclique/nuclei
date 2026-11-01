------------------------------- MODULE JobEnd -------------------------------
(***************************************************************************)
(* The end of a file's call that a blocking job makes, and the task block  *)
(* of that job (src/fs/call.rs: `write_on_pool`, `Ending`, `Slot::fill`,   *)
(* `SlotJob::poll`; src/fs/ring.rs: `finish_on_pool`; src/rt/task/raw.rs:  *)
(* `RawTask::run`, which drops a detached task's output once the task is   *)
(* gone; src/rt/blocking.rs: `Pool::run`; src/mem/mod.rs: `alloc`, `free`, *)
(* `take_back`).                                                           *)
(*                                                                          *)
(* Who acts. A task on a shard's thread writes to a file, round after      *)
(* round; each write is a job of the blocking pool, a task of its own      *)
(* whose memory is a block of the shard thread's cache. A pool thread runs *)
(* the job: it makes the call, and the job ends.                           *)
(*                                                                          *)
(* What can happen. The job's task is detached, so the run's reference is  *)
(* its last: the run frees it on the pool thread, and its block goes back  *)
(* to the shard thread's record, where the shard thread takes it back when *)
(* its own cache has none. The call's end is left in the job's slot and    *)
(* the task woken. The task looks at the slot when it is woken, and also   *)
(* once right after it issues the job: an end already there is taken at    *)
(* once, with no wake. Then the task issues its next write, which takes a  *)
(* block.                                                                  *)
(*                                                                          *)
(* What must never happen. The next write's job finds no block anywhere    *)
(* and takes one from the allocator: in steady state a write allocates     *)
(* nothing (tests/zero_alloc.rs). The write stops being counted on its    *)
(* shard before its wake went (a runtime's drop waits for the count, and   *)
(* for what a task that wake drops hands on: going/Linger.tla). The pool's *)
(* count of jobs in flight drops before the wake went (a paused clock that *)
(* sees no job in flight must see the woken task's work).                  *)
(*                                                                          *)
(* The rule that keeps the first: the end is left in the slot only once    *)
(* the job's task is gone: the job returns the end (`Ending`), and its run *)
(* drops what a detached task returned only after the task is freed, so    *)
(* whichever way the task learns of the end, the block is back first.      *)
(*                                                                          *)
(* What is abstract. One block class and one job at a time; the call, the  *)
(* run's exchanges and the free are one step (task/Task.tla checks the     *)
(* run); the waker cell is one flag, set by the task and taken by the pool *)
(* thread (the cell's own handshake is the atomic waker's, loom's part);   *)
(* the slot's own count, which the pool thread lets go of before the wake, *)
(* is left out (a slot is a spare among several).                          *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Rounds,    \* how many writes the task makes, one after another
    Warm,      \* blocks of the job's class in the shard thread's cache at the start
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm JobEnd
variables
    local = Warm,       \* blocks in the shard thread's own cache
    back = 0,           \* blocks other threads gave back to its record, not taken yet
    allocs = 0,         \* blocks the shard thread took from the allocator
    round = 0,          \* writes issued; write r ends in slot r
    done = 0,           \* writes whose end the task took
    queued = 0,         \* the write whose job waits for the pool thread, 0 for none
    full = [r \in 1..Rounds |-> FALSE],     \* slot r holds its call's end
    cell = [r \in 1..Rounds |-> FALSE],     \* slot r's waker cell holds the task's waker
    counted = [r \in 1..Rounds |-> FALSE],  \* write r is counted on the task's shard
    inFlight = [r \in 1..Rounds |-> FALSE], \* write r's job is in the pool's count
    wakeWent = [r \in 1..Rounds |-> FALSE], \* (record) write r's wake was made, or found no waker
    woken = FALSE,      \* the task was woken and will look again
    early = FALSE,      \* (record) the task found an end at its first look, with no wake
    tookBack = FALSE;   \* (record) the shard thread took a block back from its record

\* The shard thread's task: a write each round, then the end of its job.
fair process task = "task"
variables first = FALSE;    \* the next look is the first after the issue
begin
K_round:
    while round < Rounds do                                   \* round after round
K_take:
        \* RawTask::allocate: a block of the cache, else one given back, else the allocator.
        if local > 0 then                                     \* the thread's own cache has one
            local := local - 1;                               \* it takes it
        elsif back > 0 then                                   \* blocks came back from other threads
            local := back - 1;                                \* `take_back`: all of them, one used
            back := 0;                                        \* the record's list is empty again
            tookBack := TRUE;                                 \* (record)
        else                                                  \* no block anywhere
            allocs := allocs + 1;                             \* the allocator gives one
        end if;
K_issue:
        \* `write_on_pool`: a fresh slot, its waker registered first, the write counted, the job
        \* queued and counted in.
        cell[round + 1] := TRUE;                              \* the task's waker in the new slot
        counted[round + 1] := TRUE;                           \* `WriteCall::here`
        inFlight[round + 1] := TRUE;                          \* `Pool::submit` counts it in
        queued := round + 1;                                  \* and pushes it
        round := round + 1;                                   \* one more write out
        first := TRUE;                                        \* the poll right after looks first
K_look:
        \* `SlotJob::poll`: the end, if it is there, is taken at once.
        if full[round] then                                   \* the end came
            if first then early := TRUE; end if;              \* (record) with no wake at all
            goto K_got;                                       \* taken
        end if;
K_register:
        \* No end yet: the waker goes in the cell again, then a second look.
        cell[round] := TRUE;                                  \* `AtomicWaker::register`
        first := FALSE;                                       \* later looks follow a wake
K_recheck:
        if full[round] then goto K_got; end if;               \* it came meanwhile: taken
K_sleep:
        \* Pending: the task sleeps until its waker is woken (a late wake of an earlier slot
        \* wakes it too, and it looks again).
        await woken;                                          \* a wake
        woken := FALSE;                                       \* used up
        goto K_look;                                          \* and it looks again
K_got:
        done := done + 1;                                     \* the end taken: the round is over
    end while;
end process;

\* The pool thread: it runs each job, then publishes its end.
fair process pool = "pool"
variables j = 0;            \* the write whose job the thread runs
begin
P_wait:
    \* A job to run, or the task is done and none will come.
    await queued > 0 \/ done = Rounds;                        \* something to do
    if queued = 0 then goto Done; end if;                     \* no more jobs: the thread ends
P_take:
    j := queued;                                              \* the job is the thread's
    queued := 0;                                              \* and no longer waits
    \* MUTATION end_in_job: the end is left and woken inside the job, before its task is
    \* freed, as it once was.
    if Mutation = "end_in_job" then goto P_old_full; end if;  \* the old order below
P_branch:
    \* MUTATION wake_after_job: only the wake waits for the job's end; the end is left in
    \* the slot inside the job, where the task's first look can find it.
    if Mutation = "wake_after_job" then goto P_half_full; end if;   \* that order below
P_free:
    \* The run completes the detached job; its reference was the last: the task is freed
    \* and its block goes back to the shard thread's record (`mem::free`, `give_back`).
    back := back + 1;                                         \* the block is on the record's list
    \* MUTATION count_first: the job is counted out before its end is published.
    if Mutation = "count_first" then inFlight[j] := FALSE; end if;   \* too early
P_full:
    \* `Ending::drop`, the job's output dropped after the free: the end left in the slot.
    full[j] := TRUE;                                          \* `Slot::fill`: the outcome, then the flag
P_wwrite:
    \* MUTATION write_first: the write stops being counted before the wake.
    if Mutation = "write_first" then counted[j] := FALSE; end if;    \* too early
P_wake:
    \* The waker taken out of the cell after the flag, and woken.
    if cell[j] then woken := TRUE; end if;                    \* the task's waker is woken
    cell[j] := FALSE;                                         \* the cell is empty
    wakeWent[j] := TRUE;                                      \* (record) the wake went
P_write:
    \* Once the wake went: the write's count goes (`WriteCall`'s drop).
    counted[j] := FALSE;                                      \* the write is over
P_count:
    \* `Pool::run` counts the job out, after its end.
    inFlight[j] := FALSE;                                     \* the job is over
    goto P_wait;                                              \* the next job
P_old_full:
    \* (end_in_job) The end left in the slot inside the job.
    full[j] := TRUE;                                          \* the flag
P_old_wake:
    \* (end_in_job) The wake, inside the job.
    if cell[j] then woken := TRUE; end if;                    \* the task is woken
    cell[j] := FALSE;                                         \* the cell is empty
    wakeWent[j] := TRUE;                                      \* (record)
P_old_write:
    counted[j] := FALSE;                                      \* (end_in_job) the count goes
P_old_free:
    \* (end_in_job) Only now the run ends and frees the task.
    back := back + 1;                                         \* the block goes back late
P_old_count:
    inFlight[j] := FALSE;                                     \* (end_in_job) counted out
    goto P_wait;                                              \* the next job
P_half_full:
    \* (wake_after_job) The end left in the slot inside the job.
    full[j] := TRUE;                                          \* the flag, before the free
P_half_free:
    back := back + 1;                                         \* (wake_after_job) the run frees the task
P_half_wake:
    if cell[j] then woken := TRUE; end if;                    \* (wake_after_job) the wake, after the job
    cell[j] := FALSE;                                         \* the cell is empty
    wakeWent[j] := TRUE;                                      \* (record)
P_half_write:
    counted[j] := FALSE;                                      \* (wake_after_job) the count goes
P_half_count:
    inFlight[j] := FALSE;                                     \* (wake_after_job) counted out
    goto P_wait;                                              \* the next job
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties. Each says one thing the job's end promises.             *)
(***************************************************************************)

\* A write's job always finds a block in the cache or among the ones given back: a steady
\* stream of writes takes nothing from the allocator.
NoAllocation == allocs = 0

\* A write is counted on its shard until its wake went.
CountedUntilWoken == \A r \in 1..Rounds : (r <= round /\ ~counted[r]) => wakeWent[r]

\* The pool counts a job out only once its wake went.
CountedOutAfterWake == \A r \in 1..Rounds : (r <= round /\ ~inFlight[r]) => wakeWent[r]

(***************************************************************************)
(* Witnesses. Each claims an interesting case never happens; a             *)
(* configuration expects it to break, which proves the search reaches it.  *)
(***************************************************************************)

\* The task finds the end at its first look, before any wake.
NoEarlyLook == ~early
\* A block comes back to the shard thread through its record.
NoTookBack == ~tookBack
=============================================================================
