------------------------------- MODULE Interval -------------------------------
(***************************************************************************)
(* An interval's ticks (src/time/interval.rs: `Interval::poll_tick`,       *)
(* `Interval::reset`, the three `MissedTickBehavior`s), on a clock that    *)
(* moves on by itself while the task is busy elsewhere.                    *)
(*                                                                          *)
(* In plain words: an interval holds a sleep until its next tick. A poll   *)
(* of the tick finds the sleep due or not. Due, it answers the instant the *)
(* tick was due at and sets the sleep's next deadline: one period after    *)
(* that tick (burst, which catches up on every tick missed), one period    *)
(* after now (delay, which moves the schedule), or the next instant of the *)
(* schedule after now (skip, which drops the ticks missed). A reset sets   *)
(* it one period after now. A deadline past the clock's range is the far   *)
(* future instead (`far_future`), so a period too large ticks once.        *)
(*                                                                          *)
(* Who acts: the clock, which moves on by a few units at a time, and the   *)
(* task, which polls its tick whenever it gets to run. What must never     *)
(* happen: a tick answered before it was due; a deadline computed past the *)
(* clock's range (in the code, `Instant + Duration` panics there); a burst *)
(* tick off the schedule or one skipped; a skip that lands at or before    *)
(* now; a delay that comes sooner than a period after the late tick.       *)
(*                                                                          *)
(* The task's poll waits for the deadline; the clock may move any number   *)
(* of steps first, so a tick can be polled late by any amount.            *)
(*                                                                          *)
(* What is abstract: time is whole units; the sleep fires at its deadline  *)
(* (the timer families check how); the clock's range is Range, and the    *)
(* far future is the end of it.                                            *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Period,    \* the interval's period, in units
    Behavior,  \* "burst", "delay" or "skip"
    MaxStep,   \* the most the clock moves on in one step
    Horizon,   \* the clock stops here: a run is the clock's first Horizon units
    Range,     \* the last instant the clock can name; the far future is this instant
    ClampAt,   \* (only the skip_clamp mutation) the most periods a skip counts
    Resets,    \* how many resets the task may make
    Mutation   \* "none", or the name of the one rule this run takes away

\* The far future: the last instant the clock can name.
Far == Range

(* --algorithm Interval
variables
    now = 0,                 \* the clock
    deadline = 0,            \* the sleep's deadline: the next tick, the first at once
    ticks = <<>>,            \* (record) the instants the ticks answered were due at
    pastRange = FALSE,       \* (record) a deadline computed past the clock's range
    behind = FALSE,          \* (record) a skip that landed at or before now
    soon = FALSE,            \* (record) a delay sooner than a period after the late tick
    resets = 0;              \* resets made

define
    \* `checked_add` then `unwrap_or_else(far_future)` at `site`: `at` plus `by`, or the far
    \* future past the clock's range.
    \* MUTATION unchecked_add: the tick's next deadline is the sum as it is (`+`, which
    \* panics past the range in the code). MUTATION unchecked_reset: the same in `reset`.
    Add(at, by, site) ==
        IF at + by > Range /\ Mutation # site THEN Far ELSE at + by
    \* The next deadline of a tick due at `due`, polled at `t`.
    NextDeadline(due, t) ==
        CASE Behavior = "burst" -> Add(due, Period, "unchecked_add")
          [] Behavior = "delay" -> Add(t, Period, "unchecked_add")
          [] OTHER ->
               \* skip: now plus what is left of the period now falls in.
               \* MUTATION skip_clamp: the periods missed are counted, at most
               \* ClampAt of them (a u32 in the code), and the deadline is the tick
               \* plus that many periods.
               IF Mutation = "skip_clamp"
               THEN LET late == IF t > due THEN t - due ELSE 0
                        n == IF late \div Period + 1 > ClampAt THEN ClampAt ELSE late \div Period + 1
                    IN Add(due, n * Period, "unchecked_add")
               ELSE LET late == IF t > due THEN t - due ELSE 0
                        base == IF t > due THEN t ELSE due
                    IN Add(base, Period - (late % Period), "unchecked_add")
    \* Every deadline is one the clock can name.
    InRange == ~pastRange
    \* Burst: the ticks are the schedule, every instant of it, in order.
    BurstOnSchedule ==
        Behavior = "burst" /\ resets = 0 =>
            \A k \in 1..Len(ticks) : ticks[k] = (k - 1) * Period
    \* Skip: every tick on the schedule, and every next deadline after now.
    SkipAhead ==
        Behavior = "skip" =>
            /\ ~behind
            /\ resets = 0 => \A k \in 1..Len(ticks) : ticks[k] % Period = 0
    \* Delay: the next tick at least a period after the late one was answered.
    DelayGap == Behavior = "delay" => ~soon
end define;

\* The clock moves on by a few units, up to the horizon.
fair process clock = "clock"
begin
C_tick:
    while now < Horizon do
        with step \in 1..MaxStep do
            now := IF now + step > Horizon THEN Horizon ELSE now + step;
        end with;
    end while;
end process;

\* The task polls its tick whenever it runs, and may reset the interval.
fair process task = "task"
variables nx = 0;
begin
T_poll:
    while now < Horizon \/ now >= deadline do
        either
            \* `poll_tick`: the sleep due, the tick answered and the next deadline set.
            await now >= deadline;
            nx := NextDeadline(deadline, now);
            pastRange := pastRange \/ nx > Range;
            behind := behind \/ (Behavior = "skip" /\ nx <= now /\ nx # Far);
            soon := soon \/ (Behavior = "delay" /\ nx < now + Period /\ nx # Far);
            ticks := Append(ticks, deadline);
            deadline := nx;
        or
            \* `reset`: the next tick one period after now.
            await resets < Resets;
            resets := resets + 1;
            deadline := Add(now, Period, "unchecked_reset");
            pastRange := pastRange \/ Add(now, Period, "unchecked_reset") > Range;
        or
            \* The clock stopped short of the next tick: the run is over.
            await now = Horizon;
            skip;
        end either;
        if Len(ticks) >= 4 then
            goto Done;             \* a few ticks are enough to see the schedule
        end if;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
