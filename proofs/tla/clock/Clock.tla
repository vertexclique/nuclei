------------------------------- MODULE Clock -------------------------------
(***************************************************************************)
(* A paused clock that moves by itself (src/rt/paused.rs: `attempt`,      *)
(* `try_advance`; the park path in src/rt/shard/run.rs `Shard::turn` and   *)
(* src/rt/shard/timers.rs `timer_wait`, `before_park`, `after_park`; the   *)
(* job count in src/rt/blocking.rs), at the grain of                       *)
(* the atomic steps of the protocol.                                       *)
(*                                                                          *)
(* In plain words: each shard runs its work and fires its due timer. With  *)
(* nothing left it publishes its timer's deadline and counts a park, then  *)
(* says it parks and looks for work once more. Parked, it tries to move    *)
(* the clock: it claims the advance, looks at every shard (each must be    *)
(* parked with nothing pushed, noting each one's park count and deadline), *)
(* looks again (the same, unchanged), moves the clock to the earliest      *)
(* deadline, wakes the shards whose timers are due and lets the advance    *)
(* go. Then it sleeps until it is woken, unless its own timer came due. A  *)
(* shard that wakes, or that found work when it was about to sleep, waits  *)
(* for an advance in progress to end before it runs. A pusher gives a      *)
(* shard work and wakes it; a blocking job gives shard 1 work, wakes it,   *)
(* counts itself out of the jobs in flight and then tries the advance too. *)
(*                                                                          *)
(* What is modelled step by step: every load and store the protocol makes, *)
(* the look at each shard one at a time, the wake (a parked flag swapped   *)
(* back and a notification left). What is abstract: a shard's work is a    *)
(* count; running one item may arm the shard's timer (at most `Timers`     *)
(* times) some delay after the clock; the clock moves only by the jump.    *)
(*                                                                          *)
(* What is forgotten: a value no step reads again before it is next        *)
(* written goes back to its start once read for the last time (whether a   *)
(* shard's own timer came due, once it has slept; the shard a push went    *)
(* to, once it was woken). Two states that differ only there have the same *)
(* futures, so TLC walks one of them; every check sees what it saw before. *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, Sequences, TLC

CONSTANTS
    Shards,    \* the shards of the runtime, numbered 1, 2, ...
    Pushes,    \* work items a pusher gives to the shards
    Job,       \* TRUE: a blocking job is in flight at the start
    Delays,    \* the delays a timer may be armed with
    Timers,    \* timers each shard may arm in all
    Mutation   \* "none", or the name of the one rule this run takes away

Inf == 1000
NoShard == 0
Min2(a, b) == IF a < b THEN a ELSE b
Max2(a, b) == IF a > b THEN a ELSE b

(* --algorithm Clock
variables
    clock = 0,
    deadline = [s \in Shards |-> Inf],   \* the shard's armed timer
    armed = [s \in Shards |-> 0],        \* timers armed so far
    parked = [s \in Shards |-> FALSE],
    notified = [s \in Shards |-> FALSE], \* a notification left on the shard's poller
    inbox = [s \in Shards |-> 0],
    parks = [s \in Shards |-> 0],
    timerAt = [s \in Shards |-> Inf],    \* the deadline the shard published
    due = [s \in Shards |-> FALSE],      \* the shard's own attempt made its timer due
    advancing = FALSE,
    jobs = IF Job THEN 1 ELSE 0,
    pushed = 0,
    badJump = FALSE,                     \* the clock jumped while work was in flight
    jumped = FALSE;

define
    Idle(t) == parked[t] /\ inbox[t] = 0
    \* No armed timer was left behind by a jump: the clock is never past one.
    ClockNeverPassesArmed == \A s \in Shards : clock <= deadline[s]
    \* Every jump came over a moment, between its two looks, at which every shard was idle,
    \* and with no blocking job in flight.
    NoBadJump == ~badJump
    \* Witness: the clock jumps at all.
    NoJumpW == ~jumped
end define;

procedure Attempt(me)
variables todo = {}, snap = [t \in Shards |-> 0], earliest = Inf, abort = FALSE,
    idleMid = FALSE;
begin
A1:
    await ~advancing;
    advancing := TRUE || abort := FALSE || earliest := Inf || todo := Shards;
A2:
    if jobs # 0 /\ Mutation # "no_job_check" then goto A9 end if;
A3: \* the first look, one shard at a time
    while todo # {} do
        with t \in todo do
            abort := abort \/ ~Idle(t) || snap[t] := parks[t]
                || earliest := Min2(earliest, timerAt[t]) || todo := todo \ {t};
        end with;
    end while;
A4: \* (ghost) between the two looks: is every shard idle at this moment?
    idleMid := \A t \in Shards : Idle(t);
    if abort \/ earliest = Inf then goto A9 end if;
A4b:
    todo := Shards;
A5: \* the second look: the same shards, still idle, no new park
    if Mutation = "no_recheck" then goto A6 end if;
A5b:
    while todo # {} do
        with t \in todo do
            abort := abort \/ ~Idle(t) \/ parks[t] # snap[t] || todo := todo \ {t};
        end with;
    end while;
A5c:
    if abort \/ (jobs # 0 /\ Mutation # "no_job_check") then goto A9 end if;
A6:
    badJump := badJump \/ jobs # 0 \/ ~idleMid;
    jumped := jumped \/ earliest > clock;
    clock := Max2(clock, earliest);
    todo := {t \in Shards : timerAt[t] <= earliest /\ t # me};
A7: \* wake the shards whose timers came due
    while todo # {} do
        with t \in todo do
            if parked[t] then parked[t] := FALSE || notified[t] := TRUE end if;
            todo := todo \ {t};
        end with;
    end while;
A8:
    if me # NoShard then due[me] := timerAt[me] <= earliest end if;
A9:
    advancing := FALSE;
    return;
end procedure;

fair process Shard \in Shards
begin
S_run: \* run what is there: an item of work, or the due timer firing
    if inbox[self] > 0 then
        inbox[self] := inbox[self] - 1;
        goto S_arm;
    elsif deadline[self] <= clock then
        deadline[self] := Inf;
        goto S_arm;
    end if;
S_publish: \* the deadline published and the park counted before the shard says it parks
    if Mutation # "publish_late" then timerAt[self] := deadline[self] end if;
    parks[self] := parks[self] + 1;
S_park:
    parked[self] := TRUE;
S_recheck: \* one more look for work
    if inbox[self] > 0 then
        parked[self] := FALSE;
        if Mutation = "busy_no_wait" then goto S_run else goto S_wait end if;
    end if;
S_late:
    if Mutation = "publish_late" then timerAt[self] := deadline[self] end if;
S_try: \* parked: try to move the clock, unless the own timer is due already
    if deadline[self] <= clock then
        due[self] := TRUE;
    elsif Mutation # "no_attempt" then
        due[self] := FALSE;
        call Attempt(self);
    else
        due[self] := FALSE;
    end if;
S_sleep: \* until a notification (a shard with nothing left sleeps here for good)
    if ~due[self] then await notified[self] end if;
    notified[self] := FALSE;
    \* Whether the own timer came due is read here for the last time before the next
    \* attempt sets it again: forgotten, so two states that differ only in it are one.
    due[self] := FALSE;
S_end:
    parked[self] := FALSE;
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
P1:
    while pushed < Pushes do
        with t \in Shards do
            inbox[t] := inbox[t] + 1 || pushed := pushed + 1 || target := t;
        end with;
P2: \* the wake: the parked flag swapped back, a notification left
        if parked[target] then parked[target] := FALSE || notified[target] := TRUE end if;
        \* The shard just pushed to is not read again before the next push picks one:
        \* forgotten, so two states that differ only in it are one.
        target := 0;
    end while;
end process;

fair process BlockingJob = -2
begin
J1:
    if ~Job then goto Done end if;
J1b: \* the job ends, waking the task that awaits it on shard 1
    inbox[1] := inbox[1] + 1;
J2:
    if parked[1] then parked[1] := FALSE || notified[1] := TRUE end if;
J3: \* counted out after the wake
    jobs := 0;
J4:
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
