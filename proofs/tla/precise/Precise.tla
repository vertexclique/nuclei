------------------------------- MODULE Precise -------------------------------
(***************************************************************************)
(* Precise sleeps on a shard (src/time/precise.rs: `SleepPrecise`;         *)
(* src/rt/shard/timers.rs: `next_deadline`, `timer_wait`, `arm_precise`,   *)
(* `fire_timers`, `fire_precise`, `before_park`; src/rt/park/linux.rs:     *)
(* `Poller::arm_precise`, the timerfd; src/rt/park/kqueue.rs: the          *)
(* `EVFILT_TIMER`; src/rt/paused.rs: `attempt`), at the grain of one turn  *)
(* of the shard's loop.                                                    *)
(*                                                                         *)
(* In plain words: a shard keeps two timer wheels, one counting ticks (a   *)
(* millisecond in the code) and one counting the finest unit (a           *)
(* microsecond). A sleep on the tick wheel ends at its deadline rounded up *)
(* to the next tick; a precise sleep ends at its deadline itself. With     *)
(* nothing to run the shard parks until the earliest deadline of either    *)
(* wheel. Some backends wait to the finest unit by themselves (io_uring,   *)
(* Windows); the other one (epoll) waits in whole ticks, rounding up, so    *)
(* for a precise deadline it arms a timer of its own in the kernel, which  *)
(* ends the wait at the deadline itself, and it moves that timer whenever  *)
(* the earliest precise deadline changes. A precise sleep may come while   *)
(* the shard is parked: another thread wakes the shard, whose task arms   *)
(* it. On a paused clock nothing waits for real time: before it parks the *)
(* shard publishes its earliest deadline, of either wheel, and the clock   *)
(* jumps there.                                                            *)
(*                                                                         *)
(* What must never happen: a sleep that ends before its deadline, a        *)
(* precise sleep that ends after it, a sleep on the tick wheel that ends   *)
(* off its tick, or a sleep that never ends.                               *)
(*                                                                         *)
(* What is abstract: time is a count of the finest unit and a tick is four *)
(* of them; the shard does nothing but its timers, and wakes exactly when *)
(* its wait ends (no latency); kqueue's timer is the timerfd's twin, so    *)
(* the model's "coarse" backend stands for both pollers that arm one.      *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Paused,    \* TRUE: the clock is paused, and moves only by jumps to the earliest deadline
    Backend,   \* "fine": the wait itself ends to the finest unit; "coarse": it counts ticks
    Mutation   \* "none", or the name of the one rule this run takes away

\* A tick of the coarse wheel, in units of the fine one.
Tick == 4
\* No deadline: past every time the model reaches.
Never == 1000
\* A deadline on the tick wheel: rounded up to the next tick, so it never fires early.
OnTick(d) == ((d + Tick - 1) \div Tick) * Tick
Min(a, b) == IF a < b THEN a ELSE b

\* The sleeps: two precise ones and one on the tick wheel.
Sleeps == {"p1", "p2", "c"}
Precise == {"p1", "p2"}

(* --algorithm Precise
variables
    now = 0,                        \* the clock
    \* Each sleep's deadline, every choice of them; the second precise one's is set when it comes.
    deadline \in {f \in [Sleeps -> 0..9] : f["p2"] = 0 /\ f["p1"] > 0 /\ f["c"] > 0},
    armed = [s \in Sleeps |-> s # "p2"],  \* the sleep is in its wheel: the second precise one comes later
    done = [s \in Sleeps |-> FALSE],      \* the sleep ended
    doneAt = [s \in Sleeps |-> 0],        \* when it ended
    kernelAt = Never,               \* the deadline the poller's own timer is armed for, until it fires
    woken = FALSE,                  \* another thread woke the shard once, and its task armed the second precise sleep
    kernelEnded = FALSE,            \* (record) a park was ended by the poller's own timer
    moved = FALSE,                  \* (record) that timer was moved to an earlier deadline
    jumpedToPrecise = FALSE;        \* (record) a paused clock jumped to a precise deadline

define
    \* When a sleep is due: a precise one at its deadline, one on the tick wheel at its tick.
    Due(s) == IF s \in Precise THEN deadline[s] ELSE OnTick(deadline[s])
    \* The sleeps still in a wheel.
    Pending == {s \in Sleeps : armed[s] /\ ~done[s]}
    \* The earliest time a wheel has work at (`next_deadline`).
    \* MUTATION wait_tick_only: the wait looks at the tick wheel alone.
    Waited == IF Mutation = "wait_tick_only" THEN Pending \ Precise ELSE Pending
    Earliest == IF Waited = {} THEN Never
                ELSE CHOOSE t \in {Due(s) : s \in Waited} : \A s \in Waited : t <= Due(s)
    \* Whether that earliest deadline is a precise one's.
    EarliestIsPrecise == \E s \in Waited \cap Precise : Due(s) = Earliest
    \* What a paused shard publishes before it parks (`before_park`).
    \* MUTATION publish_tick_only: it publishes the tick wheel's deadline alone.
    Published == IF Mutation = "publish_tick_only"
                 THEN IF Pending \ Precise = {} THEN Never
                      ELSE CHOOSE t \in {Due(s) : s \in Pending \ Precise} :
                               \A s \in Pending \ Precise : t <= Due(s)
                 ELSE Earliest
end define;

fair process shard = "shard"
begin
S_fire:
    \* The shard fires what came due in both wheels (`fire_timers`, `fire_precise`).
    \* MUTATION fire_tick_only: the precise wheel is never fired.
    doneAt := [s \in Sleeps |-> IF s \in Pending /\ now >= Due(s)
                                   /\ ~(Mutation = "fire_tick_only" /\ s \in Precise)
                                THEN now ELSE doneAt[s]];
    done := [s \in Sleeps |-> done[s] \/ (s \in Pending /\ now >= Due(s)
                                         /\ ~(Mutation = "fire_tick_only" /\ s \in Precise))];
S_park:
    \* Every sleep ended, and the one still to come came: the model's end.
    if woken /\ \A s \in Sleeps : done[s] then
        goto Done;
    elsif Paused then
        \* A paused clock: another thread may wake the shard first, at the time the clock stands
        \* at, and its task arms the second precise sleep; else the clock jumps to the published
        \* deadline (`paused::attempt`), and with none published it never moves.
        either
            await ~woken;
            with g \in 1..2 do
                deadline["p2"] := now + g;
            end with;
            armed["p2"] := TRUE;
            woken := TRUE;
        or
            await Published < Never;
            if EarliestIsPrecise /\ Published = Earliest then
                jumpedToPrecise := TRUE;
            end if;
            now := Published;
        end either;
        goto S_fire;
    else
        \* The real clock. The wait's own timeout ends at the earliest deadline, rounded up to a
        \* tick where the backend counts ticks, and never before now; there a precise earliest
        \* deadline arms the poller's own timer, moved when the deadline is another one
        \* (`arm_precise`). MUTATION no_arm: the poller's own timer is never armed.
        \* MUTATION stale_arm: it is armed only when none is, and never moved.
        with own = (IF Earliest = Never THEN Never
                    ELSE IF Backend = "fine" THEN Earliest ELSE OnTick(Earliest)),
             arm = (Backend = "coarse" /\ EarliestIsPrecise /\ Mutation # "no_arm"
                    /\ kernelAt # Earliest
                    /\ ~(Mutation = "stale_arm" /\ kernelAt # Never)),
             prev = kernelAt,
             timer = (IF arm THEN Earliest ELSE kernelAt),
             ends = (IF Min(own, timer) < now THEN now ELSE Min(own, timer)) do
            either
                \* Another thread wakes the shard before its wait ends, and its task arms the
                \* second precise sleep, for a deadline after the wake.
                await ~woken /\ now + 1 < ends;
                with w \in now + 1 .. Min(ends - 1, now + 3), g \in 1..3 do
                    now := w;
                    deadline["p2"] := w + g;
                end with;
                armed["p2"] := TRUE;
                woken := TRUE;
                moved := moved \/ (arm /\ prev # Never /\ Earliest < prev);
                kernelAt := timer;
            or
                \* The wait ends by itself. The poller's own timer, if it ended it, fired and is
                \* armed no more; else it stays armed for its deadline.
                await ends < Never;
                now := ends;
                kernelEnded := kernelEnded \/ (timer = ends /\ timer < own);
                moved := moved \/ (arm /\ prev # Never /\ Earliest < prev);
                kernelAt := IF timer = ends THEN Never ELSE timer;
            end either;
        end with;
        goto S_fire;
    end if;
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

\* No sleep ends before its deadline.
NeverEarly == \A s \in Sleeps : done[s] => doneAt[s] >= deadline[s]

\* A precise sleep ends at its deadline itself, and a sleep on the tick wheel at its tick.
OnTime == \A s \in Sleeps : done[s] => doneAt[s] = Due(s)

Safe == NeverEarly /\ OnTime

(***************************************************************************)
(* Witnesses.                                                              *)
(***************************************************************************)

\* A park is ended by the poller's own timer.
NoKernelEnd == ~kernelEnded
\* The poller's timer is moved to an earlier precise deadline.
NoMove == ~moved
\* A paused clock jumps to a precise deadline.
NoJumpToPrecise == ~jumpedToPrecise
=============================================================================
