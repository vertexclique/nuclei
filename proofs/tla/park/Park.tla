------------------------------- MODULE Park -------------------------------
(***************************************************************************)
(* A shard going to sleep and the threads that give it work               *)
(* (src/rt/handshake.rs, src/rt/shard.rs), at the grain of the atomic      *)
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
(* not begun ends as soon as it does.                                      *)
(*                                                                          *)
(* What is abstract: the inbox is a count (the order of work does not     *)
(* matter here); the poller is one flag that a notification raises and a   *)
(* wait lowers (an eventfd's counter, collapsed).                          *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Wakers,    \* the waker threads, numbered 1, 2, ...
    TimerTask, \* TRUE: a task of the shard's own sleeps on a timer of its wheel
    ProactorWake, \* TRUE: the shard's thread is inside `Proactor::wait`, whose one
               \* wait only a `Proactor::wake` ends, and one comes
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
    driveFired = FALSE;\* (record) the timer fired in the drive on the way to park

define
    \* Every waker is done.
    WakersDone == \A w \in Wakers : pc[w] = "Done"
    \* All the work ran: the wakers' and the timer's task.
    AllRan == consumed = Total /\ local = 0 /\ ~timerArmed
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
    end if;
S_fire:
    \* The loop fires the timers that are due.
    if timerArmed /\ timerDue then
        timerArmed := FALSE;
        local := local + 1;
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
S_check:
    \* ... and only looks at the poller if that woke a task of the shard's own, which
    \* nothing else would wake it for.
    \* MUTATION drive_waits: the drive waits on the poller all the same.
    if local > 0 /\ Mutation # "drive_waits" then
        parked := FALSE;
        goto S_run;
    end if;
S_sleep:
    \* The poller's wait: until a notification, the deadline of a timer still in the wheel,
    \* or (the model's end) until every waker is done and all the work ran.
    await pending \/ (timerArmed /\ timerDue) \/ (WakersDone /\ AllRan /\ ~ProactorWake);
    if pending then
        pending := FALSE;
        parked := FALSE;                    \* `end_park`
        \* The one wait of `Proactor::wait` ended: the call returns.
        if ProactorWake then
            goto Done;
        else
            goto S_run;
        end if;
    elsif timerArmed /\ timerDue then
        parked := FALSE;
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
end process;

\* `Proactor::wake`, once, at any point of the thread's `Proactor::wait`.
fair process proactor = ProactorT
begin
P_wake:
    if ProactorWake then
        \* MUTATION handshake_wake: the wake goes through the handshake, so it
        \* notifies only a shard that parks already.
        if Mutation # "handshake_wake" then
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
        pending := TRUE;
        notifies := notifies + 1;
    end if;
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

\* All the safety properties together.
Safe == ConsumedOnce /\ OneNotifyPerPark

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
=============================================================================
