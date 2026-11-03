-------------------------------- MODULE Coop --------------------------------
(***************************************************************************)
(* The cooperative budget of nuclei::sync's receives and acquires          *)
(* (src/coop.rs: `exhausted`, `spent_one`, `spend`, `try_spend`,           *)
(* `poll_budget` and `refund`, `enter_task` and `leave_task`, `enter` and  *)
(* `leave`, `refused`; src/sync/mpsc/chan.rs: `Chan::poll_recv`;           *)
(* src/sync/contend.rs:                                                    *)
(* `taken_at_once`; src/sync/fair_semaphore.rs: `FairWait::poll_admitted`,  *)
(* its first attempt and the grant of its own queueing;                    *)
(* src/rt/task/raw.rs: `RawTask::run`, which gives each poll its budget    *)
(* and puts the one before back after it; src/rt/shard/run.rs:             *)
(* `Shard::run_loop`, whose `block_on` future gets a budget of its own per *)
(* poll; src/rt/task/join.rs: `Task::poll`, a handle awaited).             *)
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
(* between: a thread on another core, which may come at any moment. An    *)
(* acquire whose first attempt fails queues, and the pass its own queueing *)
(* runs may grant it the lock at once, if the other thread let go of it in *)
(* between: that grant completes the acquire in the same poll, so it       *)
(* spends one too, and a refused one gives the lock back the same way.     *)
(*                                                                         *)
(* A `block_on` nested in the hot task's poll ("nest") polls its own future *)
(* with a fresh budget and puts the task's back after it. An IO operation  *)
(* ("pend") is polled through `poll_budget`: it spends one, and gets it    *)
(* back when it answers Pending, so only progress spends (the task's       *)
(* future goes on with its next operation, as a `select!` does). A handle  *)
(* of a task that already finished ("join") is awaited: its output is      *)
(* taken, which cannot be undone, so the poll looks at the budget first,   *)
(* as a receive does, and spends one once it took the output; a loop over  *)
(* handles that are all finished yields like any other. (The charge of a   *)
(* Ready handle is the design the code is to follow: today's `Task::poll`  *)
(* spends nothing, which is the mutation `join_free`.)                     *)
(*                                                                         *)
(* A refusal wakes the task, which is user code when the poll's waker is   *)
(* not the runtime's own, and may panic: with WakePanics it does. The      *)
(* refused acquire gives the lock back before that wake, so a panic there  *)
(* loses nothing; the poll then unwinds and the hot task ends with it.     *)
(*                                                                         *)
(* What must never happen: a value taken and lost by a refused receive, a  *)
(* lock lost by a refused acquire, a thread left queued while the lock is  *)
(* free, a poll that completes more than its budget, the other task kept   *)
(* waiting for more than one budget, a refused task never run again, or a  *)
(* poll's budget spent by what made no progress.                           *)
(*                                                                         *)
(* What is abstract: the channel holds one value per receive from the      *)
(* start, so no send races a receive (the loom models in                   *)
(* src/sync/loom_tests check that handshake with a refusal in it); the     *)
(* lock is a semaphore of one permit whose line holds only the other       *)
(* thread, and its release and its pass are one step each (the semaphore  *)
(* family checks the line); a hot acquire that finds the lock taken and is *)
(* not granted as it queues yields and tries again on its next poll, where *)
(* the code waits in the line (a grant to a wait that queued in an earlier *)
(* poll spends nothing: that wait yielded).                                *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Budget,    \* how many operations one poll may complete
    Ops,       \* the hot task's program: a sequence of "recv" (a receive) and "acq" (a lock and its unlock)
    WakePanics,\* the refusal's wake of the hot task panics
    Mutation   \* "none", or the name of the one rule this run takes away

\* The channel starts with one value for each receive of the program, the nested future's
\* included, and each handle awaited has its output there.
Values == Len(SelectSeq(Ops, LAMBDA op : op \in {"recv", "nest", "join"}))
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
    servedByGiveBack = FALSE,    \* (record) the lock a refused acquire gave back went to the queued thread
    enqGrant = FALSE,            \* (record) an acquire was granted by the pass of its own queueing
    refunded = FALSE,            \* (record) an IO operation that answered Pending got its spend back
    joinRefused = FALSE,         \* (record) the await of a finished handle was refused for the budget
    panicked = FALSE;            \* the hot task's poll unwound from its refusal's wake

define
    \* The hot task ran its whole program, or ended with the panic of its refusal's wake.
    HotDone == next > Len(Ops) \/ panicked
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
        \* MUTATION stale_budget: the poll keeps what the last one left, so once a poll spends it
        \* every later one starts spent.
        if Mutation # "stale_budget" then left := Budget; end if;
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
    elsif Ops[next] = "nest" then
        goto S_nest;
    elsif Ops[next] = "pend" then
        goto S_pend;
    elsif Ops[next] = "join" then
        goto S_join;
    else
        goto S_acq;
    end if;
S_nest:
    \* A `block_on` inside the hot task's poll (`Shard::run_loop`): its future's poll gets a fresh
    \* budget (`coop::enter`), receives a value with it, and is Ready; the task's budget is put
    \* back after it (`coop::leave`), whatever the nested poll spent.
    \* MUTATION nest_no_restore: the task's poll goes on with what the nested poll left.
    vals := vals - 1;                       \* the nested future's receive takes a value
    got := got + 1;                         \* and has it
    if Mutation = "nest_no_restore" then
        left := Budget - 1;                 \* the fresh budget less the nested receive's spend
    end if;
    next := next + 1;                       \* the task's program goes on
    goto S_poll;
S_pend:
    \* An IO operation polled through `poll_budget`: it spends one first (`try_spend`); with the
    \* budget spent it is refused, the task woken behind the others, and the poll ends.
    if left = 0 then
        refused := TRUE;                    \* (record)
        runq := Append(runq, "hot");        \* woken behind the others
        goto S_pick;
    else
        \* The IO answers Pending: the spend is given back (`refund`), and the task's future goes
        \* on with its next operation. MUTATION no_refund: it is not, so a poll pays for what
        \* made no progress.
        if Mutation = "no_refund" then
            left := left - 1;               \* spent for nothing
        else
            refunded := TRUE;               \* (record) spent and given back
        end if;
        next := next + 1;                   \* the select goes on to its next branch
        goto S_poll;
    end if;
S_join:
    \* A handle of a task that already finished is awaited (`Task::poll`): taking the output
    \* cannot be undone, so it looks at the budget first (`coop::exhausted`); with the budget
    \* spent it answers Pending, the task woken behind the others and the output left where it
    \* is; else it takes the output and spends one (`coop::spent_one`).
    \* MUTATION join_free: it neither looks nor spends, as today's `Task::poll` does: a loop over
    \* finished handles takes every output in one poll and never yields.
    \* MUTATION join_take_then_check: it takes the output first, and a refusal throws it away.
    if Mutation = "join_take_then_check" /\ left = 0 then
        vals := vals - 1;                   \* the output taken ...
        lost := lost + 1;                   \* ... and lost
        refused := TRUE;                    \* (record)
        joinRefused := TRUE;                \* (record)
        runq := Append(runq, "hot");        \* woken behind the others
        goto S_pick;
    elsif left = 0 /\ Mutation # "join_free" then
        refused := TRUE;                    \* (record)
        joinRefused := TRUE;                \* (record)
        runq := Append(runq, "hot");        \* woken behind the others
        goto S_pick;
    else
        vals := vals - 1;                   \* the output taken
        got := got + 1;                     \* and the task's
        if Mutation # "join_free" then
            left := left - 1;               \* one spent
        end if;
        inPoll := inPoll + 1;               \* one more completed in this poll
        next := next + 1;                   \* the program goes on
        if ~otherDone then
            whileOtherWaits := whileOtherWaits + 1;   \* while the other task waits
        end if;
        goto S_poll;
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
    \* it and it is free; otherwise it queues.
    if ~wQueued /\ free > 0 then
        free := free - 1;
        hotHolds := TRUE;
    else
        goto S_enq;
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
        \* The budget is spent: the lock goes back, then the task is woken to yield behind the
        \* others (`coop::refused`). MUTATION keep_on_refuse: the refused acquire keeps the
        \* lock, which nobody will ever release. MUTATION wake_before_give_back: the wake comes
        \* first, and a wake that panics unwinds the poll before the lock went back.
        refused := TRUE;
        hotHolds := FALSE;
        if Mutation = "keep_on_refuse" then
            runq := Append(runq, "hot");
            goto S_pick;
        elsif Mutation = "wake_before_give_back" /\ WakePanics then
            panicked := TRUE;
            goto S_pick;
        else
            free := free + 1;
            giveBack := wQueued;
        end if;
    end if;
S_give_back:
    \* The give back's pass (`Lock::give_back`): the lock goes to a thread that queued for it
    \* meanwhile. MUTATION give_back_no_serve: the lock goes back free, and the queued thread
    \* is left waiting. Then the refusal's wake: the task behind the others, or with
    \* WakePanics a panic that ends the hot task's poll and the task.
    if Mutation # "give_back_no_serve" then
        Serve();
    end if;
    giveBack := FALSE;
    if WakePanics then
        panicked := TRUE;
    else
        runq := Append(runq, "hot");
    end if;
    goto S_pick;
S_enq:
    \* The acquire queues (`FairWait::poll_admitted`'s enqueue): the pass of its own queueing
    \* grants it the lock if the lock is free now and nobody waits ahead, which happens when
    \* the other thread let go of it since the first attempt. Granted, the acquire completes
    \* in this poll and spends one like an acquire taken at once. MUTATION enqueue_grant_free:
    \* the grant completes the acquire and spends nothing, as a grant to a wait that yielded.
    \* Not granted, the task yields and tries again on its next poll.
    if ~wQueued /\ free > 0 then
        free := free - 1;                   \* the pass hands the free permit to the new waiter
        hotHolds := TRUE;                   \* the hot task holds the lock
        enqGrant := TRUE;                   \* (record) granted as it queued
        if Mutation = "enqueue_grant_free" then
            inPoll := inPoll + 1;           \* the acquire completes in this poll ...
            next := next + 1;               \* ... and the program moves on ...
            if ~otherDone then
                whileOtherWaits := whileOtherWaits + 1;   \* ... while the other task waits
            end if;
            goto S_unlock;                  \* ... with nothing spent
        else
            goto S_spend;                   \* it spends one, or is refused, as a take at once is
        end if;
    else
        runq := Append(runq, "hot");        \* not granted: the task goes behind the others
        goto S_pick;
    end if;
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
VARIABLES runq, left, inPoll, next, vals, got, lost, free, hotHolds, wHolds, 
          wQueued, wGranted, otherDone, whileOtherWaits, refused, giveBack, 
          servedByGiveBack, enqGrant, refunded, joinRefused, panicked, pc

(* define statement *)
HotDone == next > Len(Ops) \/ panicked

VARIABLE t

vars == << runq, left, inPoll, next, vals, got, lost, free, hotHolds, wHolds, 
           wQueued, wGranted, otherDone, whileOtherWaits, refused, giveBack, 
           servedByGiveBack, enqGrant, refunded, joinRefused, panicked, pc, t
        >>

ProcSet == {"shard"} \cup {"w"}

Init == (* Global variables *)
        /\ runq = <<"hot", "other">>
        /\ left = Budget
        /\ inPoll = 0
        /\ next = 1
        /\ vals = Values
        /\ got = 0
        /\ lost = 0
        /\ free = Permits
        /\ hotHolds = FALSE
        /\ wHolds = FALSE
        /\ wQueued = FALSE
        /\ wGranted = FALSE
        /\ otherDone = FALSE
        /\ whileOtherWaits = 0
        /\ refused = FALSE
        /\ giveBack = FALSE
        /\ servedByGiveBack = FALSE
        /\ enqGrant = FALSE
        /\ refunded = FALSE
        /\ joinRefused = FALSE
        /\ panicked = FALSE
        (* Process shard *)
        /\ t = "none"
        /\ pc = [self \in ProcSet |-> CASE self = "shard" -> "S_pick"
                                        [] self = "w" -> "W_try"]

S_pick == /\ pc["shard"] = "S_pick"
          /\ IF HotDone /\ otherDone
                THEN /\ pc' = [pc EXCEPT !["shard"] = "Done"]
                     /\ UNCHANGED << runq, left, inPoll, t >>
                ELSE /\ runq # <<>>
                     /\ t' = Head(runq)
                     /\ runq' = Tail(runq)
                     /\ IF Mutation # "stale_budget"
                           THEN /\ left' = Budget
                           ELSE /\ TRUE
                                /\ left' = left
                     /\ inPoll' = 0
                     /\ pc' = [pc EXCEPT !["shard"] = "S_poll"]
          /\ UNCHANGED << next, vals, got, lost, free, hotHolds, wHolds, 
                          wQueued, wGranted, otherDone, whileOtherWaits, 
                          refused, giveBack, servedByGiveBack, enqGrant, 
                          refunded, joinRefused, panicked >>

S_poll == /\ pc["shard"] = "S_poll"
          /\ IF t = "other"
                THEN /\ otherDone' = TRUE
                     /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                ELSE /\ IF HotDone
                           THEN /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                           ELSE /\ IF Ops[next] = "recv"
                                      THEN /\ pc' = [pc EXCEPT !["shard"] = "S_recv"]
                                      ELSE /\ IF Ops[next] = "nest"
                                                 THEN /\ pc' = [pc EXCEPT !["shard"] = "S_nest"]
                                                 ELSE /\ IF Ops[next] = "pend"
                                                            THEN /\ pc' = [pc EXCEPT !["shard"] = "S_pend"]
                                                            ELSE /\ IF Ops[next] = "join"
                                                                       THEN /\ pc' = [pc EXCEPT !["shard"] = "S_join"]
                                                                       ELSE /\ pc' = [pc EXCEPT !["shard"] = "S_acq"]
                     /\ UNCHANGED otherDone
          /\ UNCHANGED << runq, left, inPoll, next, vals, got, lost, free, 
                          hotHolds, wHolds, wQueued, wGranted, whileOtherWaits, 
                          refused, giveBack, servedByGiveBack, enqGrant, 
                          refunded, joinRefused, panicked, t >>

S_nest == /\ pc["shard"] = "S_nest"
          /\ vals' = vals - 1
          /\ got' = got + 1
          /\ IF Mutation = "nest_no_restore"
                THEN /\ left' = Budget - 1
                ELSE /\ TRUE
                     /\ left' = left
          /\ next' = next + 1
          /\ pc' = [pc EXCEPT !["shard"] = "S_poll"]
          /\ UNCHANGED << runq, inPoll, lost, free, hotHolds, wHolds, wQueued, 
                          wGranted, otherDone, whileOtherWaits, refused, 
                          giveBack, servedByGiveBack, enqGrant, refunded, 
                          joinRefused, panicked, t >>

S_pend == /\ pc["shard"] = "S_pend"
          /\ IF left = 0
                THEN /\ refused' = TRUE
                     /\ runq' = Append(runq, "hot")
                     /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                     /\ UNCHANGED << left, next, refunded >>
                ELSE /\ IF Mutation = "no_refund"
                           THEN /\ left' = left - 1
                                /\ UNCHANGED refunded
                           ELSE /\ refunded' = TRUE
                                /\ left' = left
                     /\ next' = next + 1
                     /\ pc' = [pc EXCEPT !["shard"] = "S_poll"]
                     /\ UNCHANGED << runq, refused >>
          /\ UNCHANGED << inPoll, vals, got, lost, free, hotHolds, wHolds, 
                          wQueued, wGranted, otherDone, whileOtherWaits, 
                          giveBack, servedByGiveBack, enqGrant, joinRefused, 
                          panicked, t >>

S_join == /\ pc["shard"] = "S_join"
          /\ IF Mutation = "join_take_then_check" /\ left = 0
                THEN /\ vals' = vals - 1
                     /\ lost' = lost + 1
                     /\ refused' = TRUE
                     /\ joinRefused' = TRUE
                     /\ runq' = Append(runq, "hot")
                     /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                     /\ UNCHANGED << left, inPoll, next, got, whileOtherWaits >>
                ELSE /\ IF left = 0 /\ Mutation # "join_free"
                           THEN /\ refused' = TRUE
                                /\ joinRefused' = TRUE
                                /\ runq' = Append(runq, "hot")
                                /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                                /\ UNCHANGED << left, inPoll, next, vals, got, 
                                                whileOtherWaits >>
                           ELSE /\ vals' = vals - 1
                                /\ got' = got + 1
                                /\ IF Mutation # "join_free"
                                      THEN /\ left' = left - 1
                                      ELSE /\ TRUE
                                           /\ left' = left
                                /\ inPoll' = inPoll + 1
                                /\ next' = next + 1
                                /\ IF ~otherDone
                                      THEN /\ whileOtherWaits' = whileOtherWaits + 1
                                      ELSE /\ TRUE
                                           /\ UNCHANGED whileOtherWaits
                                /\ pc' = [pc EXCEPT !["shard"] = "S_poll"]
                                /\ UNCHANGED << runq, refused, joinRefused >>
                     /\ lost' = lost
          /\ UNCHANGED << free, hotHolds, wHolds, wQueued, wGranted, otherDone, 
                          giveBack, servedByGiveBack, enqGrant, refunded, 
                          panicked, t >>

S_recv == /\ pc["shard"] = "S_recv"
          /\ IF Mutation = "take_then_check" /\ left = 0
                THEN /\ vals' = vals - 1
                     /\ lost' = lost + 1
                     /\ refused' = TRUE
                     /\ runq' = Append(runq, "hot")
                     /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                     /\ UNCHANGED << left, inPoll, next, got, whileOtherWaits >>
                ELSE /\ IF left = 0 /\ Mutation # "no_spend"
                           THEN /\ refused' = TRUE
                                /\ IF Mutation # "no_wake"
                                      THEN /\ runq' = Append(runq, "hot")
                                      ELSE /\ TRUE
                                           /\ runq' = runq
                                /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                                /\ UNCHANGED << left, inPoll, next, vals, got, 
                                                whileOtherWaits >>
                           ELSE /\ vals' = vals - 1
                                /\ got' = got + 1
                                /\ IF Mutation # "no_spend"
                                      THEN /\ left' = left - 1
                                      ELSE /\ TRUE
                                           /\ left' = left
                                /\ inPoll' = inPoll + 1
                                /\ next' = next + 1
                                /\ IF ~otherDone
                                      THEN /\ whileOtherWaits' = whileOtherWaits + 1
                                      ELSE /\ TRUE
                                           /\ UNCHANGED whileOtherWaits
                                /\ pc' = [pc EXCEPT !["shard"] = "S_poll"]
                                /\ UNCHANGED << runq, refused >>
                     /\ lost' = lost
          /\ UNCHANGED << free, hotHolds, wHolds, wQueued, wGranted, otherDone, 
                          giveBack, servedByGiveBack, enqGrant, refunded, 
                          joinRefused, panicked, t >>

S_acq == /\ pc["shard"] = "S_acq"
         /\ IF ~wQueued /\ free > 0
               THEN /\ free' = free - 1
                    /\ hotHolds' = TRUE
                    /\ pc' = [pc EXCEPT !["shard"] = "S_spend"]
               ELSE /\ pc' = [pc EXCEPT !["shard"] = "S_enq"]
                    /\ UNCHANGED << free, hotHolds >>
         /\ UNCHANGED << runq, left, inPoll, next, vals, got, lost, wHolds, 
                         wQueued, wGranted, otherDone, whileOtherWaits, 
                         refused, giveBack, servedByGiveBack, enqGrant, 
                         refunded, joinRefused, panicked, t >>

S_spend == /\ pc["shard"] = "S_spend"
           /\ IF left > 0 \/ Mutation = "no_spend"
                 THEN /\ IF Mutation # "no_spend"
                            THEN /\ left' = left - 1
                            ELSE /\ TRUE
                                 /\ left' = left
                      /\ inPoll' = inPoll + 1
                      /\ next' = next + 1
                      /\ IF ~otherDone
                            THEN /\ whileOtherWaits' = whileOtherWaits + 1
                            ELSE /\ TRUE
                                 /\ UNCHANGED whileOtherWaits
                      /\ pc' = [pc EXCEPT !["shard"] = "S_unlock"]
                      /\ UNCHANGED << runq, free, hotHolds, refused, giveBack, 
                                      panicked >>
                 ELSE /\ refused' = TRUE
                      /\ hotHolds' = FALSE
                      /\ IF Mutation = "keep_on_refuse"
                            THEN /\ runq' = Append(runq, "hot")
                                 /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                                 /\ UNCHANGED << free, giveBack, panicked >>
                            ELSE /\ IF Mutation = "wake_before_give_back" /\ WakePanics
                                       THEN /\ panicked' = TRUE
                                            /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                                            /\ UNCHANGED << free, giveBack >>
                                       ELSE /\ free' = free + 1
                                            /\ giveBack' = wQueued
                                            /\ pc' = [pc EXCEPT !["shard"] = "S_give_back"]
                                            /\ UNCHANGED panicked
                                 /\ runq' = runq
                      /\ UNCHANGED << left, inPoll, next, whileOtherWaits >>
           /\ UNCHANGED << vals, got, lost, wHolds, wQueued, wGranted, 
                           otherDone, servedByGiveBack, enqGrant, refunded, 
                           joinRefused, t >>

S_give_back == /\ pc["shard"] = "S_give_back"
               /\ IF Mutation # "give_back_no_serve"
                     THEN /\ IF wQueued /\ free > 0
                                THEN /\ free' = free - 1
                                     /\ wQueued' = FALSE
                                     /\ wGranted' = TRUE
                                     /\ IF giveBack
                                           THEN /\ servedByGiveBack' = TRUE
                                           ELSE /\ TRUE
                                                /\ UNCHANGED servedByGiveBack
                                ELSE /\ TRUE
                                     /\ UNCHANGED << free, wQueued, wGranted, 
                                                     servedByGiveBack >>
                     ELSE /\ TRUE
                          /\ UNCHANGED << free, wQueued, wGranted, 
                                          servedByGiveBack >>
               /\ giveBack' = FALSE
               /\ IF WakePanics
                     THEN /\ panicked' = TRUE
                          /\ runq' = runq
                     ELSE /\ runq' = Append(runq, "hot")
                          /\ UNCHANGED panicked
               /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
               /\ UNCHANGED << left, inPoll, next, vals, got, lost, hotHolds, 
                               wHolds, otherDone, whileOtherWaits, refused, 
                               enqGrant, refunded, joinRefused, t >>

S_enq == /\ pc["shard"] = "S_enq"
         /\ IF ~wQueued /\ free > 0
               THEN /\ free' = free - 1
                    /\ hotHolds' = TRUE
                    /\ enqGrant' = TRUE
                    /\ IF Mutation = "enqueue_grant_free"
                          THEN /\ inPoll' = inPoll + 1
                               /\ next' = next + 1
                               /\ IF ~otherDone
                                     THEN /\ whileOtherWaits' = whileOtherWaits + 1
                                     ELSE /\ TRUE
                                          /\ UNCHANGED whileOtherWaits
                               /\ pc' = [pc EXCEPT !["shard"] = "S_unlock"]
                          ELSE /\ pc' = [pc EXCEPT !["shard"] = "S_spend"]
                               /\ UNCHANGED << inPoll, next, whileOtherWaits >>
                    /\ runq' = runq
               ELSE /\ runq' = Append(runq, "hot")
                    /\ pc' = [pc EXCEPT !["shard"] = "S_pick"]
                    /\ UNCHANGED << inPoll, next, free, hotHolds, 
                                    whileOtherWaits, enqGrant >>
         /\ UNCHANGED << left, vals, got, lost, wHolds, wQueued, wGranted, 
                         otherDone, refused, giveBack, servedByGiveBack, 
                         refunded, joinRefused, panicked, t >>

S_unlock == /\ pc["shard"] = "S_unlock"
            /\ free' = free + 1
            /\ hotHolds' = FALSE
            /\ pc' = [pc EXCEPT !["shard"] = "S_unlock_pass"]
            /\ UNCHANGED << runq, left, inPoll, next, vals, got, lost, wHolds, 
                            wQueued, wGranted, otherDone, whileOtherWaits, 
                            refused, giveBack, servedByGiveBack, enqGrant, 
                            refunded, joinRefused, panicked, t >>

S_unlock_pass == /\ pc["shard"] = "S_unlock_pass"
                 /\ IF wQueued /\ free > 0
                       THEN /\ free' = free - 1
                            /\ wQueued' = FALSE
                            /\ wGranted' = TRUE
                            /\ IF giveBack
                                  THEN /\ servedByGiveBack' = TRUE
                                  ELSE /\ TRUE
                                       /\ UNCHANGED servedByGiveBack
                       ELSE /\ TRUE
                            /\ UNCHANGED << free, wQueued, wGranted, 
                                            servedByGiveBack >>
                 /\ pc' = [pc EXCEPT !["shard"] = "S_poll"]
                 /\ UNCHANGED << runq, left, inPoll, next, vals, got, lost, 
                                 hotHolds, wHolds, otherDone, whileOtherWaits, 
                                 refused, giveBack, enqGrant, refunded, 
                                 joinRefused, panicked, t >>

shard == S_pick \/ S_poll \/ S_nest \/ S_pend \/ S_join \/ S_recv \/ S_acq
            \/ S_spend \/ S_give_back \/ S_enq \/ S_unlock \/ S_unlock_pass

W_try == /\ pc["w"] = "W_try"
         /\ IF ~UsesLock
               THEN /\ pc' = [pc EXCEPT !["w"] = "Done"]
                    /\ UNCHANGED << free, wHolds, wQueued >>
               ELSE /\ IF free > 0
                          THEN /\ free' = free - 1
                               /\ wHolds' = TRUE
                               /\ pc' = [pc EXCEPT !["w"] = "W_release"]
                               /\ UNCHANGED wQueued
                          ELSE /\ wQueued' = TRUE
                               /\ pc' = [pc EXCEPT !["w"] = "W_wait"]
                               /\ UNCHANGED << free, wHolds >>
         /\ UNCHANGED << runq, left, inPoll, next, vals, got, lost, hotHolds, 
                         wGranted, otherDone, whileOtherWaits, refused, 
                         giveBack, servedByGiveBack, enqGrant, refunded, 
                         joinRefused, panicked, t >>

W_wait == /\ pc["w"] = "W_wait"
          /\ wGranted
          /\ wHolds' = TRUE
          /\ pc' = [pc EXCEPT !["w"] = "W_release"]
          /\ UNCHANGED << runq, left, inPoll, next, vals, got, lost, free, 
                          hotHolds, wQueued, wGranted, otherDone, 
                          whileOtherWaits, refused, giveBack, servedByGiveBack, 
                          enqGrant, refunded, joinRefused, panicked, t >>

W_release == /\ pc["w"] = "W_release"
             /\ free' = free + 1
             /\ wHolds' = FALSE
             /\ pc' = [pc EXCEPT !["w"] = "Done"]
             /\ UNCHANGED << runq, left, inPoll, next, vals, got, lost, 
                             hotHolds, wQueued, wGranted, otherDone, 
                             whileOtherWaits, refused, giveBack, 
                             servedByGiveBack, enqGrant, refunded, joinRefused, 
                             panicked, t >>

other == W_try \/ W_wait \/ W_release

(* Allow infinite stuttering to prevent deadlock on termination. *)
Terminating == /\ \A self \in ProcSet: pc[self] = "Done"
               /\ UNCHANGED vars

Next == shard \/ other
           \/ Terminating

Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(shard)
        /\ WF_vars(other)

Termination == <>(\A self \in ProcSet: pc[self] = "Done")

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

\* A poll's budget is spent only by what completed in it: an operation that answered Pending
\* got its spend back.
SpentIsProgress == Budget - left <= inPoll

Safe == NoValueLost /\ Conservation /\ BoundedPoll /\ OtherWaitsOneBudget /\ NoIdleWhileQueued
        /\ SpentIsProgress

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
\* An acquire is granted by the pass of its own queueing, in the poll that queued it.
NoGrantAtEnqueue == ~enqGrant
\* An IO operation answers Pending and gets its spend back.
NoRefund == ~refunded
\* The await of a finished handle is refused for the budget: a loop over such handles yields.
NoJoinRefusal == ~joinRefused
=============================================================================
