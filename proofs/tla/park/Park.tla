------------------------------- MODULE Park -------------------------------
(***************************************************************************)
(* A shard going to sleep and the threads that give it work               *)
(* (src/rt/handshake.rs; `Shard::begin_park` and the run loop's park in   *)
(* src/rt/shard/run.rs; `Shared::push_remote` in src/rt/shard.rs), at the *)
(* grain of the atomic                                                     *)
(* steps of the handshake.                                                 *)
(*                                                                          *)
(* In plain words: a shard runs what is in its inbox. With nothing left it *)
(* publishes that it parks, looks at the inbox once more, and only then    *)
(* sleeps until its poller is notified. A waker pushes its work into the   *)
(* inbox, then looks whether the shard parks; if so it swaps the flag back *)
(* and notifies the poller. Only the waker whose swap took the flag        *)
(* notifies, so a burst of wakes costs one notification.                   *)
(*                                                                          *)
(* What is modelled step by step: the publication of `parked`, the look    *)
(* after it, the waker's push, its look at `parked` and its swap, the      *)
(* poller's pending notification and the shard's sleep. Every step is      *)
(* sequentially consistent, as the code's accesses are; that the weaker    *)
(* orderings would lose a wake is the loom calibration's part.             *)
(*                                                                          *)
(* A `Proactor::wake` (src/proactor.rs) pushes no work: it ends the wait  *)
(* of a thread inside `Proactor::wait` by leaving a notification on its   *)
(* shard's poller whether the shard parks yet or not, so a wait that has  *)
(* not begun ends as soon as it does. It reaches the shards in `WAITING`, *)
(* which `Proactor::wait` enters its shard into (`InWait`) on both of its *)
(* paths: a thread that runs no shard, and one inside a task or a          *)
(* `block_on`, whose shard runs the turn.                                  *)
(*                                                                          *)
(* A notification is a kernel call that may fail (kqueue's `kevent`, the  *)
(* port's `PostQueuedCompletionStatus`; an eventfd's write cannot):        *)
(* src/rt/park/kqueue.rs and iocp.rs `Notifier::notify`. The waker that   *)
(* took the flag back is the only one that will notify, so it tries again *)
(* until the kernel takes the notification. The code gives up after       *)
(* `NOTIFY_TRIES` failures in a row and reports the error; the model's    *)
(* failures (`NotifyFails`) stay below that.                               *)
(*                                                                          *)
(* What is abstract: the inbox is a count (the order of work does not     *)
(* matter here), which the shard's look reads exactly: that its look at   *)
(* the real queue does (both ends, not the head alone) is checked in      *)
(* `inbox` (EmptyAtSleep, MI_head_only); the poller is one flag that a     *)
(* notification raises and a wait lowers (an eventfd's counter,           *)
(* collapsed).                                                             *)
(*                                                                          *)
(* On a ring (src/rt/park/ring.rs `RingPoller::wait`) that flag ends the  *)
(* wait only while the ring's read of the eventfd, the wake's read, is in *)
(* the kernel. The shard's operations go into a submission queue of a     *)
(* fixed size; past it they wait in the poller, the wake's read first in  *)
(* line, and an enter hands the kernel only what the queue holds. So a    *)
(* wait that may sleep hands the kernel every entry first. A `block_on`   *)
(* future's own timer, fired by the drive on the way to park, wakes the   *)
(* shard through the handshake (src/rt/shard/signal.rs `wake_by_ref`),    *)
(* not through its run queue: the drive then waits, and only the          *)
(* notification ends that wait. Before, an operation burst larger than    *)
(* the queue (130 idle receives on a ring of 64 entries) left the wake's  *)
(* read out while the shard slept with no deadline, and the future's      *)
(* notification, or any thread's, ended nothing (src/rt/shard/ring/       *)
(* tests_cap.rs hung in CI). Abstract: the kernel takes every entry it is *)
(* handed; where it takes not all, the code's wait only looks instead.    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Wakers,    \* the waker threads, numbered 1, 2, ...
    TimerTask, \* TRUE: a task of the shard's own sleeps on a timer of its wheel
    ProactorWake, \* TRUE: the shard's thread is inside `Proactor::wait`, whose one
               \* wait only a `Proactor::wake` ends, and one comes
    TimerFuture, \* TRUE: the shard's `block_on` future sleeps on a timer of its wheel, and
               \* its waker wakes the shard through the handshake, not its run queue
    FullQueue, \* TRUE: the shard's operations fill its ring's submission queue before it
               \* parks, so the wake's read waits behind them, outside the kernel
    NotifyFails, \* how many notifications' kernel calls fail, in all
    Mutation   \* "none", or the name of the one rule this run takes away

\* The shard's thread number: no waker uses it.
ShardT == 0
\* The clock's: it makes the timer due.
ClockT == -1
\* The thread that calls `Proactor::wake`.
ProactorT == -2
\* Every item of work: one per waker.
Total == Cardinality(Wakers)

(* --algorithm Park
variables
    parked = FALSE,    \* the shard published that it parks
    inbox = 0,         \* work pushed and not run yet
    pending = FALSE,   \* the poller has a notification the shard has not taken
    consumed = 0,      \* (record) work the shard ran
    notifies = 0,      \* (record) notifications sent
    rechecked = FALSE, \* (record) the look after publishing found work
    spurious = FALSE,  \* (record) a notification came while the shard was awake
    timerArmed = TimerTask, \* the task's timer is in the wheel, not fired
    timerDue = FALSE,  \* its deadline passed
    local = 0,         \* the shard's own runnable tasks (the timer's, once it fired)
    driveFired = FALSE,\* (record) the timer fired in the drive on the way to park
    futureArmed = TimerFuture, \* the future's timer is in the wheel, not fired
    futureDue = FALSE, \* its deadline passed
    futureWoken = FALSE, \* the future's signal says it was woken, so the loop polls it
    wakeIn = ~FullQueue, \* the wake's read is in the kernel, where a notification ends a wait
    failed = 0,        \* notifications whose kernel call failed so far
    outFired = FALSE;  \* (record) the drive fired the future's timer while the wake's read was out

define
    \* Every waker is done.
    WakersDone == \A w \in Wakers : pc[w] = "Done"
    \* All the work ran: the wakers', the timer's task and the future woken by its timer.
    AllRan == consumed = Total /\ local = 0 /\ ~timerArmed /\ ~futureArmed /\ ~futureWoken
end define;

fair process shard = ShardT
begin
S_run:
    \* Run what is in the inbox, and the shard's own tasks.
    if inbox > 0 then
        inbox := inbox - 1;
        consumed := consumed + 1;
        goto S_run;
    elsif local > 0 then
        local := local - 1;
        goto S_run;
    elsif futureWoken then
        \* The future's signal says it was woken: the loop polls it, and it finishes.
        futureWoken := FALSE;
        goto S_run;
    end if;
S_fire:
    \* The loop fires the timers that are due.
    if timerArmed /\ timerDue then
        timerArmed := FALSE;
        local := local + 1;
        goto S_run;
    elsif futureArmed /\ futureDue then
        \* The future's timer: its waker marks the signal (the shard does not park, so the
        \* handshake notifies nothing), and the loop polls the future.
        futureArmed := FALSE;
        futureWoken := TRUE;
        goto S_run;
    end if;
S_publish:
    \* `begin_park`: publish that the shard parks ...
    parked := TRUE;
S_look:
    \* ... then look once more.
    \* MUTATION no_recheck: the shard sleeps without looking again.
    if inbox > 0 /\ Mutation # "no_recheck" then
        rechecked := TRUE;                  \* (record)
        parked := FALSE;
        goto S_run;
    end if;
S_drive:
    \* `drive`: it fires the timers that came due meanwhile ...
    if timerArmed /\ timerDue then
        timerArmed := FALSE;
        local := local + 1;
        driveFired := TRUE;                 \* (record)
    end if;
    \* ... the future's among them: its waker marks the signal, and the shard parking, takes
    \* the flag back and notifies the poller, as any waker does.
    if futureArmed /\ futureDue then
        futureArmed := FALSE;
        futureWoken := TRUE;
        if ~wakeIn then outFired := TRUE; end if; \* (record)
        if parked then
            parked := FALSE;
            pending := TRUE;
        end if;
    end if;
S_check:
    \* ... and only looks at the poller if that woke a task of the shard's own, which
    \* nothing else would wake it for.
    \* MUTATION drive_waits: the drive waits on the poller all the same.
    if local > 0 /\ Mutation # "drive_waits" then
        parked := FALSE;
        goto S_run;
    end if;
S_submit:
    \* The wait that may sleep hands the kernel every entry first, the wake's read among them.
    \* MUTATION no_submit_first: the enter hands the kernel only what the submission queue
    \* holds, so behind a full queue the wake's read stays out while the shard sleeps.
    if Mutation # "no_submit_first" then
        wakeIn := TRUE;
    end if;
S_sleep:
    \* The poller's wait: until a notification the wake's read sees, the deadline of a timer
    \* still in the wheel, or (the model's end) until every waker is done and all the work ran.
    await (pending /\ wakeIn) \/ (timerArmed /\ timerDue) \/ (futureArmed /\ futureDue)
          \/ (WakersDone /\ AllRan /\ ~ProactorWake);
    if pending /\ wakeIn then
        pending := FALSE;
        parked := FALSE;                    \* `end_park`
        \* The one wait of `Proactor::wait` ended: the call returns.
        if ProactorWake then
            goto Done;
        else
            goto S_run;
        end if;
    elsif (timerArmed /\ timerDue) \/ (futureArmed /\ futureDue) then
        parked := FALSE;
        \* The wait ended at a deadline: what waited behind the queue goes in at the next one.
        wakeIn := TRUE;
        goto S_run;
    end if;
end process;

\* The clock: the timer's deadline passes at some point.
fair process clock = ClockT
begin
C_tick:
    if timerArmed then
        timerDue := TRUE;
    end if;
    \* The future's deadline passes at the same tick (the model needs one moment only).
    if futureArmed then
        futureDue := TRUE;
    end if;
end process;

\* `Proactor::wake`, once, at any point of the thread's `Proactor::wait`.
fair process proactor = ProactorT
begin
P_wake:
    if ProactorWake then
        \* MUTATION unregistered_wait: the wait made on a thread that runs a shard (inside a
        \* task or a `block_on`) is not entered into `WAITING`, so the wake, which walks
        \* `WAITING`, ends nothing.
        if Mutation = "unregistered_wait" then
            skip;
        \* MUTATION handshake_wake: the wake goes through the handshake, so it
        \* notifies only a shard that parks already.
        elsif Mutation # "handshake_wake" then
            pending := TRUE;
        elsif parked then
            parked := FALSE;
            pending := TRUE;
        end if;
    end if;
end process;

fair process waker \in Wakers
begin
W_start:
    \* MUTATION check_first: the waker looks at `parked` before it pushes its work.
    if Mutation = "check_first" then goto W_look; end if;
W_push:
    \* The work is made visible.
    inbox := inbox + 1;
    if Mutation = "check_first" then goto Done; end if;
W_look:
    \* `wake`: does the shard park?
    if parked then goto W_swap;
    elsif Mutation = "check_first" then goto W_push;
    else goto Done;
    end if;
W_swap:
    \* The swap: only the waker that takes the flag back notifies.
    if parked then
        parked := FALSE;
        if pc[ShardT] # "S_sleep" then spurious := TRUE; end if;   \* (record)
    else
        goto W_end;
    end if;
W_notify:
    \* The notification's kernel call. While failures are left it may fail: the waker
    \* tries again, as no other waker will notify the shard it took the flag back from.
    if failed < NotifyFails then
        either
            failed := failed + 1;
            \* MUTATION notify_swallowed: a failed notification is dropped and the waker
            \* goes on, the shard asleep with nobody left to wake it.
            if Mutation = "notify_swallowed" then goto W_end; else goto W_notify; end if;
        or
            pending := TRUE;
            notifies := notifies + 1;
        end either;
    else
        \* No failure left: the kernel takes it.
        pending := TRUE;
        notifies := notifies + 1;
    end if;
W_end:
    \* (check_first) The waker pushes its work only now.
    if Mutation = "check_first" then goto W_push; end if;
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

\* No work is run twice or made up.
ConsumedOnce == consumed <= Total /\ inbox >= 0

\* One notification per park at most: every notification took the flag back first.
OneNotifyPerPark == notifies <= Total

\* A shard asleep with a notification left for it has the wake's read in the kernel to see it:
\* otherwise only an operation's completion could end the wait, and an idle peer sends none.
SleepSeesWake == (pc[ShardT] = "S_sleep" /\ pending) => wakeIn

\* All the safety properties together.
Safe == ConsumedOnce /\ OneNotifyPerPark /\ SleepSeesWake

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A waker notifies the shard.
NoNotify == notifies = 0
\* The shard's look after publishing finds the work and it stays awake.
NoRecheckFind == ~rechecked
\* A notification lands while the shard is awake (harmless: its next wait returns at once).
NoSpurious == ~spurious
\* The timer comes due on the shard's way to park, and the drive fires it.
NoDriveFire == ~driveFired
\* The future's timer comes due on the shard's way to park while the wake's read is out behind a
\* full queue, and the drive fires it.
NoOutFire == ~outFired
=============================================================================
