-------------------------------- MODULE Coop --------------------------------
(***************************************************************************)
(* The cooperative budget of nuclei::sync's receives and acquires          *)
(* (src/coop.rs: `exhausted`, `spent_one`, `spend`, `try_spend`;           *)
(* src/sync/mpsc/chan.rs: `Chan::poll_recv`; src/sync/contend.rs:          *)
(* `taken_at_once`; src/sync/fair_semaphore.rs: `FairWait::poll_admitted`;  *)
(* src/rt/task/raw.rs: `RawTask::run`, which gives each poll its budget).  *)
(*                                                                         *)
(* In plain words: one shard runs its tasks one poll at a time, from a     *)
(* line. Each poll starts with a budget of `Budget` operations. A hot task *)
(* keeps receiving from a channel that always has a value, or taking a     *)
(* lock that is always free, so it would never wait and never let the      *)
(* other task of its shard run. With the budget, every receive or acquire  *)
(* that completes spends one; when none is left, the next one completes    *)
(* nothing, puts its task at the back of the line, and the poll ends. A    *)
(* receive looks at the budget before it takes a value, so a refused one   *)
(* takes nothing. An acquire takes the lock first and looks after, so a    *)
(* refused one gives the lock back, and serves whoever queued for it in    *)
(* between: a thread on another core, which may come at any moment.       *)
(*                                                                         *)
(* What must never happen: a value taken and lost by a refused receive, a  *)
(* lock lost by a refused acquire, a thread left queued while the lock is  *)
(* free, a poll that completes more than its budget, the other task kept   *)
(* waiting for more than one budget, or a refused task never run again.    *)
(*                                                                         *)
(* What is abstract: the channel holds one value per receive from the      *)
(* start, so no send races a receive (the loom models in                   *)
(* src/sync/loom_tests check that handshake with a refusal in it); the     *)
(* lock is a semaphore of one permit whose line holds only the other       *)
(* thread, and its release and its pass are one step each (the semaphore  *)
(* family checks the line); a hot acquire that finds the lock taken yields *)
(* and tries again on its next poll, where the code waits in the line.     *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Budget,    \* how many operations one poll may complete
    Ops,       \* the hot task's program: a sequence of "recv" (a receive) and "acq" (a lock and its unlock)
    Mutation   \* "none", or the name of the one rule this run takes away

\* The channel starts with one value for each receive of the program.
Values == Len(SelectSeq(Ops, LAMBDA op : op = "recv"))
\* The lock has one permit.
Permits == 1
\* The other thread takes the lock once when the program has acquires.
UsesLock == Len(SelectSeq(Ops, LAMBDA op : op = "acq")) > 0

(* --algorithm Coop
variables
    runq = <<"hot", "other">>,   \* the shard's line of tasks to poll: the hot task first, then the other one
    left = Budget,               \* the budget left in the poll that runs now
    inPoll = 0,                  \* operations the hot task completed in the poll that runs now
    next = 1,                    \* the hot task's next operation, an index into Ops
    vals = Values,               \* values waiting in the channel
    got = 0,                     \* values the hot task received
    lost = 0,                    \* values taken and thrown away
    free = Permits,              \* the lock's free permits
    hotHolds = FALSE,            \* the hot task holds the lock
    wHolds = FALSE,              \* the other thread holds the lock
    wQueued = FALSE,             \* the other thread waits in the lock's line
    wGranted = FALSE,            \* the lock was handed to the other thread in its line
    otherDone = FALSE,           \* the other task had its poll
    whileOtherWaits = 0,         \* operations the hot task completed while the other task waited for its turn
    refused = FALSE,             \* (record) an operation was refused for the budget
    giveBack = FALSE,            \* (record) a refused acquire gave the lock back while the thread was queued
    servedByGiveBack = FALSE;    \* (record) the lock a refused acquire gave back went to the queued thread

define
    \* The hot task ran its whole program.
    HotDone == next > Len(Ops)
end define;

\* The lock's pass: a free permit goes to the thread waiting in the line, which is woken.
macro Serve() begin
    if wQueued /\ free > 0 then
        free := free - 1;
        wQueued := FALSE;
        wGranted := TRUE;
        if giveBack then servedByGiveBack := TRUE; end if;
    end if;
end macro;

\* The shard's thread: it polls the task at the front of its line.
fair process shard = "shard"
variables t = "none";   \* the task this poll is of
begin
S_pick:
    \* The shard is done once both tasks are; until then it waits for a task in its line.
    if HotDone /\ otherDone then
        goto Done;
    else
        await runq # <<>>;
        \* The front task is polled, with a fresh budget (`RawTask::run`, `coop::enter_task`).
        t := Head(runq);
        runq := Tail(runq);
        left := Budget;
        inPoll := 0;
    end if;
S_poll:
    if t = "other" then
        \* The other task needs one poll, and is done.
        otherDone := TRUE;
        goto S_pick;
    elsif HotDone then
        \* The hot task's program ended: its future completed, and it is not queued again.
        goto S_pick;
    elsif Ops[next] = "recv" then
        goto S_recv;
    else
        goto S_acq;
    end if;
S_recv:
    \* A receive (`Chan::poll_recv`): it looks at the budget first (`coop::exhausted`).
    \* MUTATION take_then_check: it takes the value first, and a refusal throws it away.
    if Mutation = "take_then_check" /\ left = 0 then
        vals := vals - 1;
        lost := lost + 1;
        refused := TRUE;
        runq := Append(runq, "hot");
        goto S_pick;
    elsif left = 0 /\ Mutation # "no_spend" then
        \* The budget is spent: nothing is taken, the task is woken behind the others, and the
        \* poll ends. MUTATION no_wake: the task is not woken, so nobody polls it again.
        refused := TRUE;
        if Mutation # "no_wake" then
            runq := Append(runq, "hot");
        end if;
        goto S_pick;
    else
        \* A value is there: it is taken, and one operation is spent (`coop::spent_one`).
        \* MUTATION no_spend: the receive spends nothing, and never refuses.
        vals := vals - 1;
        got := got + 1;
        if Mutation # "no_spend" then
            left := left - 1;
        end if;
        inPoll := inPoll + 1;
        next := next + 1;
        if ~otherDone then
            whileOtherWaits := whileOtherWaits + 1;
        end if;
        goto S_poll;
    end if;
S_acq:
    \* An acquire's first attempt (`Contend`, `FairWait`): it takes the lock if nobody waits for
    \* it and it is free; otherwise the task yields and tries again on its next poll.
    if ~wQueued /\ free > 0 then
        free := free - 1;
        hotHolds := TRUE;
    else
        runq := Append(runq, "hot");
        goto S_pick;
    end if;
S_spend:
    \* Taken at once: one operation is spent (`taken_at_once`, `coop::spend`).
    if left > 0 \/ Mutation = "no_spend" then
        if Mutation # "no_spend" then
            left := left - 1;
        end if;
        inPoll := inPoll + 1;
        next := next + 1;
        if ~otherDone then
            whileOtherWaits := whileOtherWaits + 1;
        end if;
        goto S_unlock;
    else
        \* The budget is spent: the lock goes back and the task yields behind the others.
        \* MUTATION keep_on_refuse: the refused acquire keeps the lock, which nobody will
        \* ever release.
        refused := TRUE;
        hotHolds := FALSE;
        runq := Append(runq, "hot");
        if Mutation # "keep_on_refuse" then
            free := free + 1;
            giveBack := wQueued;
        else
            goto S_pick;
        end if;
    end if;
S_give_back:
    \* The give back's pass (`Lock::give_back`): the lock goes to a thread that queued for it
    \* meanwhile. MUTATION give_back_no_serve: the lock goes back free, and the queued thread
    \* is left waiting.
    if Mutation # "give_back_no_serve" then
        Serve();
    end if;
    giveBack := FALSE;
    goto S_pick;
S_unlock:
    \* The hot task's critical section ends: the lock is released.
    free := free + 1;
    hotHolds := FALSE;
S_unlock_pass:
    \* The release's pass serves the line.
    Serve();
    goto S_poll;
end process;

\* A thread on another core takes the lock once, as the hot task's program runs.
fair process other = "w"
begin
W_try:
    \* A program with no acquire leaves it nothing to do; else its first attempt takes the free
    \* lock, or it queues in the line.
    if ~UsesLock then
        goto Done;
    elsif free > 0 then
        free := free - 1;
        wHolds := TRUE;
        goto W_release;
    else
        wQueued := TRUE;
    end if;
W_wait:
    \* It waits until the lock is handed to it.
    await wGranted;
    wHolds := TRUE;
W_release:
    \* It releases the lock; nobody else waits in the line.
    free := free + 1;
    wHolds := FALSE;
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

\* No value is lost: every value is still in the channel or was received.
NoValueLost == vals + got = Values /\ lost = 0

\* No lock is lost: the permit is free, held by one of the two, or handed to the queued thread
\* and not taken up yet.
Conservation ==
    free + (IF hotHolds THEN 1 ELSE 0) + (IF wHolds THEN 1 ELSE 0)
        + (IF wGranted /\ ~wHolds /\ pc["w"] = "W_wait" THEN 1 ELSE 0) = Permits

\* A poll completes at most its budget of operations.
BoundedPoll == inPoll <= Budget

\* The other task waits for its turn while the hot task completes at most one budget.
OtherWaitsOneBudget == whileOtherWaits <= Budget

\* No thread waits in the line while the lock is free, once no pass is due (the shard is between
\* polls, or done).
NoIdleWhileQueued == (pc["shard"] \in {"S_pick", "Done"}) => ~(wQueued /\ free > 0)

Safe == NoValueLost /\ Conservation /\ BoundedPoll /\ OtherWaitsOneBudget /\ NoIdleWhileQueued

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* An operation is refused for the budget.
NoRefusal == ~refused
\* The other task runs between two polls of the hot task, before its program ends.
NoOtherBetween == ~(otherDone /\ next > 1 /\ ~HotDone)
\* A refused acquire gives the lock back to the thread that queued while the hot task held it.
NoServeByGiveBack == ~servedByGiveBack
=============================================================================
