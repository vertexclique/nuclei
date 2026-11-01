------------------------------- MODULE Clock -------------------------------
(***************************************************************************)
(* A paused clock that moves by itself (src/rt/paused.rs, the park path in *)
(* src/rt/shard.rs, the job count in src/rt/blocking.rs), at the grain of  *)
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
\* BEGIN TRANSLATION
CONSTANT defaultInitValue
VARIABLES clock, deadline, armed, parked, notified, inbox, parks, timerAt, 
          due, advancing, jobs, pushed, badJump, jumped, pc, stack

(* define statement *)
Idle(t) == parked[t] /\ inbox[t] = 0

ClockNeverPassesArmed == \A s \in Shards : clock <= deadline[s]


NoBadJump == ~badJump

NoJumpW == ~jumped

VARIABLES me, todo, snap, earliest, abort, idleMid, target

vars == << clock, deadline, armed, parked, notified, inbox, parks, timerAt, 
           due, advancing, jobs, pushed, badJump, jumped, pc, stack, me, todo, 
           snap, earliest, abort, idleMid, target >>

ProcSet == (Shards) \cup {-1} \cup {-2}

Init == (* Global variables *)
        /\ clock = 0
        /\ deadline = [s \in Shards |-> Inf]
        /\ armed = [s \in Shards |-> 0]
        /\ parked = [s \in Shards |-> FALSE]
        /\ notified = [s \in Shards |-> FALSE]
        /\ inbox = [s \in Shards |-> 0]
        /\ parks = [s \in Shards |-> 0]
        /\ timerAt = [s \in Shards |-> Inf]
        /\ due = [s \in Shards |-> FALSE]
        /\ advancing = FALSE
        /\ jobs = IF Job THEN 1 ELSE 0
        /\ pushed = 0
        /\ badJump = FALSE
        /\ jumped = FALSE
        (* Procedure Attempt *)
        /\ me = [ self \in ProcSet |-> defaultInitValue]
        /\ todo = [ self \in ProcSet |-> {}]
        /\ snap = [ self \in ProcSet |-> [t \in Shards |-> 0]]
        /\ earliest = [ self \in ProcSet |-> Inf]
        /\ abort = [ self \in ProcSet |-> FALSE]
        /\ idleMid = [ self \in ProcSet |-> FALSE]
        (* Process Pusher *)
        /\ target = 0
        /\ stack = [self \in ProcSet |-> << >>]
        /\ pc = [self \in ProcSet |-> CASE self \in Shards -> "S_run"
                                        [] self = -1 -> "P1"
                                        [] self = -2 -> "J1"]

A1(self) == /\ pc[self] = "A1"
            /\ ~advancing
            /\ /\ abort' = [abort EXCEPT ![self] = FALSE]
               /\ advancing' = TRUE
               /\ earliest' = [earliest EXCEPT ![self] = Inf]
               /\ todo' = [todo EXCEPT ![self] = Shards]
            /\ pc' = [pc EXCEPT ![self] = "A2"]
            /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                            parks, timerAt, due, jobs, pushed, badJump, jumped, 
                            stack, me, snap, idleMid, target >>

A2(self) == /\ pc[self] = "A2"
            /\ IF jobs # 0 /\ Mutation # "no_job_check"
                  THEN /\ pc' = [pc EXCEPT ![self] = "A9"]
                  ELSE /\ pc' = [pc EXCEPT ![self] = "A3"]
            /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                            parks, timerAt, due, advancing, jobs, pushed, 
                            badJump, jumped, stack, me, todo, snap, earliest, 
                            abort, idleMid, target >>

A3(self) == /\ pc[self] = "A3"
            /\ IF todo[self] # {}
                  THEN /\ \E t \in todo[self]:
                            /\ abort' = [abort EXCEPT ![self] = abort[self] \/ ~Idle(t)]
                            /\ earliest' = [earliest EXCEPT ![self] = Min2(earliest[self], timerAt[t])]
                            /\ snap' = [snap EXCEPT ![self][t] = parks[t]]
                            /\ todo' = [todo EXCEPT ![self] = todo[self] \ {t}]
                       /\ pc' = [pc EXCEPT ![self] = "A3"]
                  ELSE /\ pc' = [pc EXCEPT ![self] = "A4"]
                       /\ UNCHANGED << todo, snap, earliest, abort >>
            /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                            parks, timerAt, due, advancing, jobs, pushed, 
                            badJump, jumped, stack, me, idleMid, target >>

A4(self) == /\ pc[self] = "A4"
            /\ idleMid' = [idleMid EXCEPT ![self] = \A t \in Shards : Idle(t)]
            /\ IF abort[self] \/ earliest[self] = Inf
                  THEN /\ pc' = [pc EXCEPT ![self] = "A9"]
                  ELSE /\ pc' = [pc EXCEPT ![self] = "A4b"]
            /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                            parks, timerAt, due, advancing, jobs, pushed, 
                            badJump, jumped, stack, me, todo, snap, earliest, 
                            abort, target >>

A4b(self) == /\ pc[self] = "A4b"
             /\ todo' = [todo EXCEPT ![self] = Shards]
             /\ pc' = [pc EXCEPT ![self] = "A5"]
             /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                             parks, timerAt, due, advancing, jobs, pushed, 
                             badJump, jumped, stack, me, snap, earliest, abort, 
                             idleMid, target >>

A5(self) == /\ pc[self] = "A5"
            /\ IF Mutation = "no_recheck"
                  THEN /\ pc' = [pc EXCEPT ![self] = "A6"]
                  ELSE /\ pc' = [pc EXCEPT ![self] = "A5b"]
            /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                            parks, timerAt, due, advancing, jobs, pushed, 
                            badJump, jumped, stack, me, todo, snap, earliest, 
                            abort, idleMid, target >>

A5b(self) == /\ pc[self] = "A5b"
             /\ IF todo[self] # {}
                   THEN /\ \E t \in todo[self]:
                             /\ abort' = [abort EXCEPT ![self] = abort[self] \/ ~Idle(t) \/ parks[t] # snap[self][t]]
                             /\ todo' = [todo EXCEPT ![self] = todo[self] \ {t}]
                        /\ pc' = [pc EXCEPT ![self] = "A5b"]
                   ELSE /\ pc' = [pc EXCEPT ![self] = "A5c"]
                        /\ UNCHANGED << todo, abort >>
             /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                             parks, timerAt, due, advancing, jobs, pushed, 
                             badJump, jumped, stack, me, snap, earliest, 
                             idleMid, target >>

A5c(self) == /\ pc[self] = "A5c"
             /\ IF abort[self] \/ (jobs # 0 /\ Mutation # "no_job_check")
                   THEN /\ pc' = [pc EXCEPT ![self] = "A9"]
                   ELSE /\ pc' = [pc EXCEPT ![self] = "A6"]
             /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                             parks, timerAt, due, advancing, jobs, pushed, 
                             badJump, jumped, stack, me, todo, snap, earliest, 
                             abort, idleMid, target >>

A6(self) == /\ pc[self] = "A6"
            /\ badJump' = (badJump \/ jobs # 0 \/ ~idleMid[self])
            /\ jumped' = (jumped \/ earliest[self] > clock)
            /\ clock' = Max2(clock, earliest[self])
            /\ todo' = [todo EXCEPT ![self] = {t \in Shards : timerAt[t] <= earliest[self] /\ t # me[self]}]
            /\ pc' = [pc EXCEPT ![self] = "A7"]
            /\ UNCHANGED << deadline, armed, parked, notified, inbox, parks, 
                            timerAt, due, advancing, jobs, pushed, stack, me, 
                            snap, earliest, abort, idleMid, target >>

A7(self) == /\ pc[self] = "A7"
            /\ IF todo[self] # {}
                  THEN /\ \E t \in todo[self]:
                            /\ IF parked[t]
                                  THEN /\ /\ notified' = [notified EXCEPT ![t] = TRUE]
                                          /\ parked' = [parked EXCEPT ![t] = FALSE]
                                  ELSE /\ TRUE
                                       /\ UNCHANGED << parked, notified >>
                            /\ todo' = [todo EXCEPT ![self] = todo[self] \ {t}]
                       /\ pc' = [pc EXCEPT ![self] = "A7"]
                  ELSE /\ pc' = [pc EXCEPT ![self] = "A8"]
                       /\ UNCHANGED << parked, notified, todo >>
            /\ UNCHANGED << clock, deadline, armed, inbox, parks, timerAt, due, 
                            advancing, jobs, pushed, badJump, jumped, stack, 
                            me, snap, earliest, abort, idleMid, target >>

A8(self) == /\ pc[self] = "A8"
            /\ IF me[self] # NoShard
                  THEN /\ due' = [due EXCEPT ![me[self]] = timerAt[me[self]] <= earliest[self]]
                  ELSE /\ TRUE
                       /\ due' = due
            /\ pc' = [pc EXCEPT ![self] = "A9"]
            /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                            parks, timerAt, advancing, jobs, pushed, badJump, 
                            jumped, stack, me, todo, snap, earliest, abort, 
                            idleMid, target >>

A9(self) == /\ pc[self] = "A9"
            /\ advancing' = FALSE
            /\ pc' = [pc EXCEPT ![self] = Head(stack[self]).pc]
            /\ todo' = [todo EXCEPT ![self] = Head(stack[self]).todo]
            /\ snap' = [snap EXCEPT ![self] = Head(stack[self]).snap]
            /\ earliest' = [earliest EXCEPT ![self] = Head(stack[self]).earliest]
            /\ abort' = [abort EXCEPT ![self] = Head(stack[self]).abort]
            /\ idleMid' = [idleMid EXCEPT ![self] = Head(stack[self]).idleMid]
            /\ me' = [me EXCEPT ![self] = Head(stack[self]).me]
            /\ stack' = [stack EXCEPT ![self] = Tail(stack[self])]
            /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                            parks, timerAt, due, jobs, pushed, badJump, jumped, 
                            target >>

Attempt(self) == A1(self) \/ A2(self) \/ A3(self) \/ A4(self) \/ A4b(self)
                    \/ A5(self) \/ A5b(self) \/ A5c(self) \/ A6(self)
                    \/ A7(self) \/ A8(self) \/ A9(self)

S_run(self) == /\ pc[self] = "S_run"
               /\ IF inbox[self] > 0
                     THEN /\ inbox' = [inbox EXCEPT ![self] = inbox[self] - 1]
                          /\ pc' = [pc EXCEPT ![self] = "S_arm"]
                          /\ UNCHANGED deadline
                     ELSE /\ IF deadline[self] <= clock
                                THEN /\ deadline' = [deadline EXCEPT ![self] = Inf]
                                     /\ pc' = [pc EXCEPT ![self] = "S_arm"]
                                ELSE /\ pc' = [pc EXCEPT ![self] = "S_publish"]
                                     /\ UNCHANGED deadline
                          /\ inbox' = inbox
               /\ UNCHANGED << clock, armed, parked, notified, parks, timerAt, 
                               due, advancing, jobs, pushed, badJump, jumped, 
                               stack, me, todo, snap, earliest, abort, idleMid, 
                               target >>

S_publish(self) == /\ pc[self] = "S_publish"
                   /\ IF Mutation # "publish_late"
                         THEN /\ timerAt' = [timerAt EXCEPT ![self] = deadline[self]]
                         ELSE /\ TRUE
                              /\ UNCHANGED timerAt
                   /\ parks' = [parks EXCEPT ![self] = parks[self] + 1]
                   /\ pc' = [pc EXCEPT ![self] = "S_park"]
                   /\ UNCHANGED << clock, deadline, armed, parked, notified, 
                                   inbox, due, advancing, jobs, pushed, 
                                   badJump, jumped, stack, me, todo, snap, 
                                   earliest, abort, idleMid, target >>

S_park(self) == /\ pc[self] = "S_park"
                /\ parked' = [parked EXCEPT ![self] = TRUE]
                /\ pc' = [pc EXCEPT ![self] = "S_recheck"]
                /\ UNCHANGED << clock, deadline, armed, notified, inbox, parks, 
                                timerAt, due, advancing, jobs, pushed, badJump, 
                                jumped, stack, me, todo, snap, earliest, abort, 
                                idleMid, target >>

S_recheck(self) == /\ pc[self] = "S_recheck"
                   /\ IF inbox[self] > 0
                         THEN /\ parked' = [parked EXCEPT ![self] = FALSE]
                              /\ IF Mutation = "busy_no_wait"
                                    THEN /\ pc' = [pc EXCEPT ![self] = "S_run"]
                                    ELSE /\ pc' = [pc EXCEPT ![self] = "S_wait"]
                         ELSE /\ pc' = [pc EXCEPT ![self] = "S_late"]
                              /\ UNCHANGED parked
                   /\ UNCHANGED << clock, deadline, armed, notified, inbox, 
                                   parks, timerAt, due, advancing, jobs, 
                                   pushed, badJump, jumped, stack, me, todo, 
                                   snap, earliest, abort, idleMid, target >>

S_late(self) == /\ pc[self] = "S_late"
                /\ IF Mutation = "publish_late"
                      THEN /\ timerAt' = [timerAt EXCEPT ![self] = deadline[self]]
                      ELSE /\ TRUE
                           /\ UNCHANGED timerAt
                /\ pc' = [pc EXCEPT ![self] = "S_try"]
                /\ UNCHANGED << clock, deadline, armed, parked, notified, 
                                inbox, parks, due, advancing, jobs, pushed, 
                                badJump, jumped, stack, me, todo, snap, 
                                earliest, abort, idleMid, target >>

S_try(self) == /\ pc[self] = "S_try"
               /\ IF deadline[self] <= clock
                     THEN /\ due' = [due EXCEPT ![self] = TRUE]
                          /\ pc' = [pc EXCEPT ![self] = "S_sleep"]
                          /\ UNCHANGED << stack, me, todo, snap, earliest, 
                                          abort, idleMid >>
                     ELSE /\ IF Mutation # "no_attempt"
                                THEN /\ due' = [due EXCEPT ![self] = FALSE]
                                     /\ /\ me' = [me EXCEPT ![self] = self]
                                        /\ stack' = [stack EXCEPT ![self] = << [ procedure |->  "Attempt",
                                                                                 pc        |->  "S_sleep",
                                                                                 todo      |->  todo[self],
                                                                                 snap      |->  snap[self],
                                                                                 earliest  |->  earliest[self],
                                                                                 abort     |->  abort[self],
                                                                                 idleMid   |->  idleMid[self],
                                                                                 me        |->  me[self] ] >>
                                                                             \o stack[self]]
                                     /\ todo' = [todo EXCEPT ![self] = {}]
                                     /\ snap' = [snap EXCEPT ![self] = [t \in Shards |-> 0]]
                                     /\ earliest' = [earliest EXCEPT ![self] = Inf]
                                     /\ abort' = [abort EXCEPT ![self] = FALSE]
                                     /\ idleMid' = [idleMid EXCEPT ![self] = FALSE]
                                     /\ pc' = [pc EXCEPT ![self] = "A1"]
                                ELSE /\ due' = [due EXCEPT ![self] = FALSE]
                                     /\ pc' = [pc EXCEPT ![self] = "S_sleep"]
                                     /\ UNCHANGED << stack, me, todo, snap, 
                                                     earliest, abort, idleMid >>
               /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, 
                               parks, timerAt, advancing, jobs, pushed, 
                               badJump, jumped, target >>

S_sleep(self) == /\ pc[self] = "S_sleep"
                 /\ IF ~due[self]
                       THEN /\ notified[self]
                       ELSE /\ TRUE
                 /\ notified' = [notified EXCEPT ![self] = FALSE]
                 /\ pc' = [pc EXCEPT ![self] = "S_end"]
                 /\ UNCHANGED << clock, deadline, armed, parked, inbox, parks, 
                                 timerAt, due, advancing, jobs, pushed, 
                                 badJump, jumped, stack, me, todo, snap, 
                                 earliest, abort, idleMid, target >>

S_end(self) == /\ pc[self] = "S_end"
               /\ parked' = [parked EXCEPT ![self] = FALSE]
               /\ pc' = [pc EXCEPT ![self] = "S_wait"]
               /\ UNCHANGED << clock, deadline, armed, notified, inbox, parks, 
                               timerAt, due, advancing, jobs, pushed, badJump, 
                               jumped, stack, me, todo, snap, earliest, abort, 
                               idleMid, target >>

S_wait(self) == /\ pc[self] = "S_wait"
                /\ IF Mutation # "no_wait_advance"
                      THEN /\ ~advancing
                      ELSE /\ TRUE
                /\ pc' = [pc EXCEPT ![self] = "S_run"]
                /\ UNCHANGED << clock, deadline, armed, parked, notified, 
                                inbox, parks, timerAt, due, advancing, jobs, 
                                pushed, badJump, jumped, stack, me, todo, snap, 
                                earliest, abort, idleMid, target >>

S_arm(self) == /\ pc[self] = "S_arm"
               /\ \/ /\ TRUE
                     /\ UNCHANGED <<deadline, armed>>
                  \/ /\ armed[self] < Timers /\ deadline[self] = Inf
                     /\ \E d \in Delays:
                          /\ armed' = [armed EXCEPT ![self] = armed[self] + 1]
                          /\ deadline' = [deadline EXCEPT ![self] = clock + d]
               /\ pc' = [pc EXCEPT ![self] = "S_run"]
               /\ UNCHANGED << clock, parked, notified, inbox, parks, timerAt, 
                               due, advancing, jobs, pushed, badJump, jumped, 
                               stack, me, todo, snap, earliest, abort, idleMid, 
                               target >>

Shard(self) == S_run(self) \/ S_publish(self) \/ S_park(self)
                  \/ S_recheck(self) \/ S_late(self) \/ S_try(self)
                  \/ S_sleep(self) \/ S_end(self) \/ S_wait(self)
                  \/ S_arm(self)

P1 == /\ pc[-1] = "P1"
      /\ IF pushed < Pushes
            THEN /\ \E t \in Shards:
                      /\ inbox' = [inbox EXCEPT ![t] = inbox[t] + 1]
                      /\ pushed' = pushed + 1
                      /\ target' = t
                 /\ pc' = [pc EXCEPT ![-1] = "P2"]
            ELSE /\ pc' = [pc EXCEPT ![-1] = "Done"]
                 /\ UNCHANGED << inbox, pushed, target >>
      /\ UNCHANGED << clock, deadline, armed, parked, notified, parks, timerAt, 
                      due, advancing, jobs, badJump, jumped, stack, me, todo, 
                      snap, earliest, abort, idleMid >>

P2 == /\ pc[-1] = "P2"
      /\ IF parked[target]
            THEN /\ /\ notified' = [notified EXCEPT ![target] = TRUE]
                    /\ parked' = [parked EXCEPT ![target] = FALSE]
            ELSE /\ TRUE
                 /\ UNCHANGED << parked, notified >>
      /\ pc' = [pc EXCEPT ![-1] = "P1"]
      /\ UNCHANGED << clock, deadline, armed, inbox, parks, timerAt, due, 
                      advancing, jobs, pushed, badJump, jumped, stack, me, 
                      todo, snap, earliest, abort, idleMid, target >>

Pusher == P1 \/ P2

J1 == /\ pc[-2] = "J1"
      /\ IF ~Job
            THEN /\ pc' = [pc EXCEPT ![-2] = "Done"]
            ELSE /\ pc' = [pc EXCEPT ![-2] = "J1b"]
      /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, parks, 
                      timerAt, due, advancing, jobs, pushed, badJump, jumped, 
                      stack, me, todo, snap, earliest, abort, idleMid, target >>

J1b == /\ pc[-2] = "J1b"
       /\ inbox' = [inbox EXCEPT ![1] = inbox[1] + 1]
       /\ pc' = [pc EXCEPT ![-2] = "J2"]
       /\ UNCHANGED << clock, deadline, armed, parked, notified, parks, 
                       timerAt, due, advancing, jobs, pushed, badJump, jumped, 
                       stack, me, todo, snap, earliest, abort, idleMid, target >>

J2 == /\ pc[-2] = "J2"
      /\ IF parked[1]
            THEN /\ /\ notified' = [notified EXCEPT ![1] = TRUE]
                    /\ parked' = [parked EXCEPT ![1] = FALSE]
            ELSE /\ TRUE
                 /\ UNCHANGED << parked, notified >>
      /\ pc' = [pc EXCEPT ![-2] = "J3"]
      /\ UNCHANGED << clock, deadline, armed, inbox, parks, timerAt, due, 
                      advancing, jobs, pushed, badJump, jumped, stack, me, 
                      todo, snap, earliest, abort, idleMid, target >>

J3 == /\ pc[-2] = "J3"
      /\ jobs' = 0
      /\ pc' = [pc EXCEPT ![-2] = "J4"]
      /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, parks, 
                      timerAt, due, advancing, pushed, badJump, jumped, stack, 
                      me, todo, snap, earliest, abort, idleMid, target >>

J4 == /\ pc[-2] = "J4"
      /\ /\ me' = [me EXCEPT ![-2] = NoShard]
         /\ stack' = [stack EXCEPT ![-2] = << [ procedure |->  "Attempt",
                                                pc        |->  "Done",
                                                todo      |->  todo[-2],
                                                snap      |->  snap[-2],
                                                earliest  |->  earliest[-2],
                                                abort     |->  abort[-2],
                                                idleMid   |->  idleMid[-2],
                                                me        |->  me[-2] ] >>
                                            \o stack[-2]]
      /\ todo' = [todo EXCEPT ![-2] = {}]
      /\ snap' = [snap EXCEPT ![-2] = [t \in Shards |-> 0]]
      /\ earliest' = [earliest EXCEPT ![-2] = Inf]
      /\ abort' = [abort EXCEPT ![-2] = FALSE]
      /\ idleMid' = [idleMid EXCEPT ![-2] = FALSE]
      /\ pc' = [pc EXCEPT ![-2] = "A1"]
      /\ UNCHANGED << clock, deadline, armed, parked, notified, inbox, parks, 
                      timerAt, due, advancing, jobs, pushed, badJump, jumped, 
                      target >>

BlockingJob == J1 \/ J1b \/ J2 \/ J3 \/ J4

(* Allow infinite stuttering to prevent deadlock on termination. *)
Terminating == /\ \A self \in ProcSet: pc[self] = "Done"
               /\ UNCHANGED vars

Next == Pusher \/ BlockingJob
           \/ (\E self \in ProcSet: Attempt(self))
           \/ (\E self \in Shards: Shard(self))
           \/ Terminating

Spec == /\ Init /\ [][Next]_vars
        /\ \A self \in Shards : WF_vars(Shard(self)) /\ WF_vars(Attempt(self))
        /\ WF_vars(Pusher)
        /\ WF_vars(BlockingJob) /\ WF_vars(Attempt(-2))

Termination == <>(\A self \in ProcSet: pc[self] = "Done")

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
