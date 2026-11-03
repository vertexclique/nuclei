------------------------------- MODULE Clock -------------------------------
(***************************************************************************)
(* A paused clock that moves by itself (src/rt/paused.rs: `try_advance`,   *)
(* `attempt`; src/rt/shard/run.rs: `Shard::run_loop`, the park path;       *)
(* src/rt/shard/timers.rs: `before_park`, `timer_wait`, `after_park`,      *)
(* `fire_timers`; src/rt/shard.rs: `Shared::post_timer`, `Shared::idle`;   *)
(* src/time/sleep.rs: `TimerOp::needs_wake`; src/rt/blocking.rs: the job   *)
(* count; src/rt/handoff/monitor.rs: `take`, the count of polls taken from *)
(* their thread; src/rt/workers.rs: `run_shard`, `finish_home`,            *)
(* `Workers::count_out_taken_here`; src/rt/paused.rs: `Guest::join`,       *)
(* `Guest::leave`), at the grain of the atomic steps of the protocol.      *)
(*                                                                          *)
(* In plain words: each shard runs its work and fires its due timer. With  *)
(* nothing left it publishes its timer's deadline and counts a park, then  *)
(* says it parks and looks for work once more. Parked, it tries to move    *)
(* the clock: it claims the advance and looks at the jobs in flight, looks *)
(* at every shard the clock counts (each must be parked with nothing       *)
(* pushed or posted to it, noting each one's park count and deadline),    *)
(* looks at the jobs again and at the shards again (the same, unchanged),  *)
(* moves the clock to the earliest deadline, wakes the shards whose timers *)
(* are due and lets the advance go. Then it sleeps until it is woken,      *)
(* unless its own timer came due. A shard that wakes, or that found work   *)
(* when it was about to sleep, waits for an advance in progress to end     *)
(* before it runs. A pusher gives a shard work and wakes it. A poster      *)
(* gives a shard a timer op (a removal or a cancel, posted by a thread     *)
(* that is not the shard's) and wakes it too: a shard holding an op is not *)
(* idle, so a parked one left holding it would hold every jump up. Work in *)
(* flight on no shard the clock counts (a blocking job, or the rest of a   *)
(* poll the handoff took from its thread, counted alike) gives shard 1     *)
(* work, wakes it, counts itself out of the work in flight and then tries  *)
(* the advance too. A taken poll's thread that goes on to its own shard    *)
(* (a `block_on`, `finish_on_own`, `finish_home`) makes the shard a guest  *)
(* before it counts the poll out, and counts it in again before the guest  *)
(* leaves: the work is in the count or on a counted shard that is not idle *)
(* at every moment, as the job's item pushed before the job counts out is. *)
(* The thread counts it out for good once its bound tasks ended.           *)
(*                                                                          *)
(* What must never happen: the clock jumps past an armed timer, or jumps   *)
(* while some shard has work or work is in flight; and an armed timer      *)
(* never fires.                                                            *)
(*                                                                          *)
(* A shard the clock does not look at (`Late`) is a witness of an open     *)
(* question, not fixed: a thread's own shard inside `block_on` since       *)
(* before a pause from another thread, whose timer then never fires.       *)
(*                                                                          *)
(* What is modelled step by step: every load and store the protocol makes, *)
(* the look at each shard one at a time (ending at the first that is not   *)
(* idle, as the code's loops return), the wake (a parked flag swapped back *)
(* and a notification left). What is abstract: a shard's work is a count;  *)
(* running one item may arm the shard's timer (at most `Timers` times)     *)
(* some delay after the clock; the clock moves only by the jump; a timer   *)
(* op is a count, which the shard's next look applies and which arms       *)
(* nothing. Steps that touch only the attempt's own variables are merged   *)
(* with the shared access next to them; a claim of the advance that finds  *)
(* jobs in flight lets it go at once, which no other step can tell from    *)
(* never claiming it. A shard's park count and the attempt's note of it    *)
(* are one flag per shard (`still`): the attempt's first look at the shard *)
(* raises it and a park lowers it, so the second look finds it lowered     *)
(* exactly when the code finds the count changed; one attempt looks at a   *)
(* time (it holds the advance), and the flags go back to their start as it *)
(* ends.                                                                   *)
(*                                                                          *)
(* What is forgotten: a value no step reads again before it is next        *)
(* written goes back to its start once read for the last time (whether a   *)
(* shard's own timer came due, once it has slept; the shard a push or a    *)
(* post went to, once it was woken; the deadline a shard published, once  *)
(* it woke: an attempt that looks at a shard that is not parked ends       *)
(* there, without its deadline, and a woken shard waits for the attempt to *)
(* end before it parks again). Two states that differ only there have the  *)
(* same futures, so TLC walks one of them; every check sees what it saw    *)
(* before.                                                                 *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, Sequences, TLC

CONSTANTS
    Shards,    \* the shards that run the runtime's tasks, numbered 1, 2, ... or model values
    Late,      \* those the clock does not count (does not look at): {} for none
    Pushes,    \* work items a pusher gives to the shards
    PushTo,    \* the shards the pusher may give an item to
    Posts,     \* timer ops another thread posts to the shards
    PostTo,    \* the shards the poster may post an op to
    Job,       \* TRUE: work is in flight at the start, which ends by giving shard 1 an item
    Delays,    \* the delays a timer may be armed with
    Timers,    \* timers each shard may arm in all
    Mutation   \* "none", or the name of the one rule this run takes away

\* No deadline: past every time the model reaches.
Inf == 1000
\* The attempt of a thread that runs no shard (the blocking job's).
NoShard == 0
\* The smaller and the larger of two numbers.
Min2(a, b) == IF a < b THEN a ELSE b
Max2(a, b) == IF a > b THEN a ELSE b
\* The shards the clock looks at: the runtime's workers and its counted guests.
Counted == Shards \ Late

(* --algorithm Clock
variables
    clock = 0,                           \* the paused clock's time
    deadline = [s \in Shards |-> Inf],   \* the shard's armed timer, Inf for none
    armed = [s \in Shards |-> 0],        \* timers the shard armed so far
    parked = [s \in Shards |-> FALSE],   \* the shard said it parks (its park flag)
    notified = [s \in Shards |-> FALSE], \* a notification left on the shard's poller
    inbox = [s \in Shards |-> 0],        \* work items pushed to the shard, not run yet
    ops = [s \in Shards |-> 0],          \* timer ops posted to the shard, not applied yet
    \* the running attempt's first look found the shard idle, and it began no park since
    still = [s \in Shards |-> FALSE],
    timerAt = [s \in Shards |-> Inf],    \* the deadline the shard published
    due = [s \in Shards |-> FALSE],      \* the shard's own attempt made its timer due
    advancing = FALSE,                   \* an attempt holds the advance
    \* the count of work in flight an attempt reads. MUTATION detached_uncounted: a poll the
    \* handoff took from its thread is left out of it, as before the fix.
    jobs = IF Job /\ Mutation # "detached_uncounted" THEN 1 ELSE 0,
    inFlight = Job,                      \* (ghost) the work is in flight, counted or not
    pushed = 0,                          \* items the pusher gave so far
    posted = 0,                          \* ops the poster posted so far
    badJump = FALSE,                     \* (record) the clock jumped while something was busy
    jumped = FALSE;                      \* (record) the clock jumped at all

define
    \* Idle: parked, with nothing pushed or posted to it (`Shared::idle`).
    Idle(t) == parked[t] /\ inbox[t] = 0 /\ ops[t] = 0
    \* No armed timer was left behind by a jump: the clock is never past one.
    ClockNeverPassesArmed == \A s \in Shards : clock <= deadline[s]
    \* Every jump came over a moment, between its two looks, at which every shard was idle
    \* (counted or not), and with no work in flight.
    NoBadJump == ~badJump
    \* Witness: the clock jumps at all.
    NoJumpW == ~jumped
end define;

\* `paused::attempt`, by shard `me` (or by the job, `NoShard`), the advance claimed first.
procedure Attempt(me)
variables todo = {}, earliest = Inf, abort = FALSE, idleMid = FALSE;
begin
A1: \* claim the advance (one attempt at a time), and look at the jobs in flight: with one,
    \* the advance is let go at once, as if never claimed
    await ~advancing;
    if jobs # 0 /\ Mutation # "no_job_check" then
        return;
    else
        advancing := TRUE || todo := Counted;
    end if;
A3: \* the first look, one shard at a time: each must be idle; its park count (its flag
    \* raised) and deadline noted; the look ends at the first that is not idle
    while todo # {} do
        with t \in todo do
            abort := ~Idle(t) || still[t] := TRUE
                || earliest := Min2(earliest, timerAt[t])
                || todo := IF Idle(t) THEN todo \ {t} ELSE {};
        end with;
    end while;
A4: \* (ghost) between the looks: is every shard idle at this moment, counted or not? Then a
    \* shard not idle, or no deadline to move to, ends the attempt; else the jobs once more
    \* (before the second look, as the code's `unchanged` reads them first)
    idleMid := \A t \in Shards : Idle(t);
    if abort \/ earliest = Inf \/ (jobs # 0 /\ Mutation # "no_job_check") then
        goto A9;
    elsif Mutation = "no_recheck" then
        goto A6;
    else
        todo := Counted;
    end if;
A5: \* the second look: the same shards, still idle, no new park; it ends at the first change
    while todo # {} do
        with t \in todo do
            abort := ~Idle(t) \/ ~still[t]
                || todo := IF Idle(t) /\ still[t] THEN todo \ {t} ELSE {};
        end with;
    end while;
A6: \* a change seen ends the attempt; else the jump: the clock moves to the earliest deadline
    if abort then
        goto A9;
    else
        badJump := badJump \/ inFlight \/ ~idleMid;
        jumped := jumped \/ earliest > clock;
        clock := Max2(clock, earliest);
        todo := Counted \ {me};
    end if;
A7: \* wake each counted shard whose published deadline came due, read as the wake reaches it
    while todo # {} do
        with t \in todo do
            if parked[t] /\ timerAt[t] <= earliest then
                parked[t] := FALSE || notified[t] := TRUE;
            end if;
            todo := todo \ {t};
        end with;
    end while;
A8: \* the attempting shard, if counted, notes whether its own timer came due; the advance goes,
    \* and its notes of the park counts with it (forgotten)
    if me \in Counted then due[me] := timerAt[me] <= earliest end if;
    advancing := FALSE || still := [s \in Shards |-> FALSE];
    return;
A9: \* an attempt that did not jump lets the advance go, its notes of the park counts with it
    advancing := FALSE || still := [s \in Shards |-> FALSE];
    return;
end procedure;

fair process Shard \in Shards
begin
S_run: \* the look: the ops posted to it applied (they arm nothing), then what is there run:
       \* an item of work, or the due timer firing
    ops[self] := 0;
    if inbox[self] > 0 then
        inbox[self] := inbox[self] - 1;
        goto S_arm;
    elsif deadline[self] <= clock then
        deadline[self] := Inf;
        goto S_arm;
    end if;
S_publish: \* the deadline published and the park counted before the shard says it parks
    if Mutation # "publish_late" then timerAt[self] := deadline[self] end if;
    still[self] := FALSE;
S_park: \* the shard says it parks
    parked[self] := TRUE;
S_recheck: \* one more look for work pushed or posted; a shard that found some runs again
    if inbox[self] > 0 \/ ops[self] > 0 then
        parked[self] := FALSE;
        if Mutation = "busy_no_wait" then goto S_run else goto S_wait end if;
    elsif Mutation = "publish_late" then
        \* MUTATION publish_late: the deadline published only now, after the park.
        timerAt[self] := deadline[self];
    end if;
S_try: \* parked: try to move the clock, unless the own timer is due already
    if deadline[self] <= clock then
        due[self] := TRUE;
    elsif Mutation # "no_attempt" then
        call Attempt(self);
    end if;
S_sleep: \* until a notification (a shard with nothing left sleeps here for good)
    if ~due[self] then await notified[self] end if;
    notified[self] := FALSE;
    \* Whether the own timer came due is read here for the last time before the next
    \* attempt sets it again: forgotten, so two states that differ only in it are one.
    due[self] := FALSE;
S_end: \* the park ends; the deadline it published is read by no one any more (forgotten)
    parked[self] := FALSE || timerAt[self] := Inf;
S_wait: \* an advance in progress ends before the shard reads the clock
    if Mutation # "no_wait_advance" then await ~advancing end if;
    goto S_run;
S_arm: \* the work just run, or the task the timer woke, may arm the timer
    either skip
    or
        await armed[self] < Timers /\ deadline[self] = Inf;
        with d \in Delays do
            deadline[self] := clock + d || armed[self] := armed[self] + 1;
        end with;
    end either;
    goto S_run;
end process;

fair process Pusher = -1
variable target = 0;
begin
P1: \* give one item of work to one of the shards it may push to
    while pushed < Pushes do
        with t \in PushTo do
            inbox[t] := inbox[t] + 1 || pushed := pushed + 1 || target := t;
        end with;
P2: \* the wake: the parked flag swapped back, a notification left
        if parked[target] then parked[target] := FALSE || notified[target] := TRUE end if;
        \* The shard just pushed to is not read again before the next push picks one:
        \* forgotten, so two states that differ only in it are one.
        target := 0;
    end while;
end process;

fair process Poster = -3
variable dest = 0;
begin
Q1: \* post a timer op (a removal or a cancel) to one of the shards it may post to
    while posted < Posts do
        with t \in PostTo do
            ops[t] := ops[t] + 1 || posted := posted + 1 || dest := t;
        end with;
Q2: \* the wake, as for an arm request (`TimerOp::needs_wake` while a clock is paused)
        \* MUTATION post_no_wake: today's rule before the fix, a removal or a cancel waking nobody.
        if Mutation # "post_no_wake" /\ parked[dest] then
            parked[dest] := FALSE || notified[dest] := TRUE;
        end if;
        \* The shard just posted to is not read again before the next post picks one:
        \* forgotten, so two states that differ only in it are one.
        dest := 0;
    end while;
end process;

fair process BlockingJob = -2
begin
J1: \* the work ends (if there is any), waking the task that awaits it on shard 1: pushed there
    if ~Job then goto Done else inbox[1] := inbox[1] + 1 end if;
J2: \* the wake of shard 1
    if parked[1] then parked[1] := FALSE || notified[1] := TRUE end if;
J3: \* counted out after the wake, and out of flight
    jobs := 0 || inFlight := FALSE;
J4: \* then it tries the advance itself
    call Attempt(NoShard);
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

\* Every armed timer eventually fires: a runtime whose shards all sleep with a timer armed and
\* nothing to wake them hangs, and breaks this. (A quiet runtime, every shard asleep with no timer,
\* is a final state: the configurations do not check deadlock.)
\* Written as "from every moment on, a moment comes (now or later) at which the shard has no timer
\* armed", which is the same promise as "always, once a timer is armed, it later fires": from a
\* moment with no timer armed, that moment itself is the one; from a moment with one armed, the
\* moment it fires is. And a timer that never fired would leave no such moment after it was armed.
\* TLC checks this form directly, without the extra automaton the other form needs, so it costs less.
TimersFire == \A s \in Shards : []<>(deadline[s] = Inf)
=============================================================================
