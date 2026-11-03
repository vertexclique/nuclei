------------------------------ MODULE ClockRead ------------------------------
(***************************************************************************)
(* The readings of a runtime's clock as it is paused (src/time/clock.rs:   *)
(* `Clock::now`, `Clock::now_from`, `Clock::now_nanos_from`,               *)
(* `Clock::pause`, and the process-wide `now` and `now_nanos`), at the     *)
(* grain of each load, store and reading of the system's clock.            *)
(*                                                                          *)
(* In plain words: threads read the clock again and again, and each one's  *)
(* readings must never go back. A clock nobody paused or advanced is the   *)
(* system's own: a reading is the system's time, and leaves no trace. Once *)
(* the clock is marked (paused or advanced, ever), a reading leaves its    *)
(* trace: it takes the later of itself and `last`, the latest reading      *)
(* given, and puts itself into `last`. A pause marks the clock, reads the  *)
(* time it stops at, stores it as the paused time, says it is paused, and  *)
(* raises the paused time to `last`, which a reading between those stores *)
(* may have passed.                                                        *)
(*                                                                          *)
(* What must never happen: a thread gets a reading earlier than one it got *)
(* before.                                                                 *)
(*                                                                          *)
(* A reading that leaves its trace is never earlier than one before it:   *)
(* it gives at least `last`, which holds every one before. A reading of an *)
(* unmarked clock leaves none, so two rules keep it from being later than *)
(* the time a pause then stops at, each taken away by a mutation:          *)
(*   - an unmarked reading reads the system's time before it looks at the  *)
(*     mark: a reading that finds the clock unmarked took its time before  *)
(*     the pause marked it, so before the time the pause stops at;         *)
(*   - the pause marks the clock before it reads that time.                *)
(*                                                                          *)
(* What is abstract: time is a count the system's clock moves on by one,   *)
(* up to `MaxTime`; the pause rounds its time up to the millisecond in the *)
(* code, which only makes the paused time later, so here it does not      *)
(* round (the worst case); one pause and no resume or advance (once the    *)
(* clock is marked, every reading leaves its trace, and those are kept in  *)
(* order by `last`); the steps run in one order every thread agrees on.   *)
(* The pause's accesses are sequentially consistent, a fence after its     *)
(* mark. A reader's look at the mark is a plain load after the call that   *)
(* reads the system's clock, which the compiler does not move ahead of the *)
(* call; that the processor does not either is assumed here, not shown.    *)
(* The system's clock is read through the vDSO, whose counter reads are    *)
(* ordered: on x86-64 `rdtsc_ordered` (an `lfence` before `rdtsc`, or      *)
(* `rdtscp`), on arm64 an `isb` with `arch_counter_enforce_ordering`, which *)
(* makes later loads wait for the counter, so the processor keeps the look *)
(* at the mark after the read too. The Rust memory model gives no such    *)
(* order between a call and a relaxed load, so Miri's weak-memory          *)
(* emulation may read the mark early: its run of                          *)
(* `reads_never_go_backwards_across_pauses_on_many_threads` turns that     *)
(* emulation off (`-Zmiri-disable-weak-memory-emulation`), and this model, *)
(* whose steps run in one order every thread agrees on, is the check of    *)
(* the rule itself.                                                        *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Readers,   \* the threads that read the clock, numbered 1, 2, ...
    Reads,     \* how many readings each thread takes
    MaxTime,   \* the last time the system's clock reaches in a run
    Mutation   \* "none", or the name of the one rule this run takes away

\* The larger of two numbers.
Max2(a, b) == IF a > b THEN a ELSE b

(* --algorithm ClockRead
variables
    sys = 0,                       \* the system's monotonic clock
    altered = FALSE,               \* the clock's mark: paused or advanced, ever
    paused = FALSE,                \* the clock stands still
    base = 0,                      \* paused: the time it stands at
    last = 0,                      \* the latest reading a marked clock gave
    gave = [r \in Readers |-> 0],  \* each thread's latest reading
    back = FALSE;                  \* (record) a thread got a reading earlier than its one before

define
    \* Every thread's readings never go back.
    ReaderMonotonic == ~back
end define;

\* Process names are numbers, as the readers' are: TLC compares them with one another.
fair process Time = -1
begin
T_tick: \* the system's clock moves on, one unit at a time, up to the end of the run
    while sys < MaxTime do
        sys := sys + 1;
    end while;
end process;

fair process Reader \in Readers
variables n = 0, t = 0, run = FALSE;
begin
R_loop: \* `Clock::now`, `Reads` times
    while n < Reads do
        if Mutation = "check_then_read" then
            \* MUTATION check_then_read: the mark looked at first, as before the fix; a
            \* marked clock is read the recorded way.
            if altered then goto R_paused end if;
R_late:     \* then the system's time, whenever the thread gets to it: the reading
            t := sys;
            goto R_give;
        else
            \* the system's time read first
            t := sys;
        end if;
R_mark: \* then the mark: an unmarked clock's reading is that time
        if ~altered then goto R_give end if;
R_paused: \* `Clock::now_nanos`: the look at whether the clock is paused
        run := ~paused;
R_value: \* the paused time, or, running, the system's time read first (a marked running clock
         \* adds a skew, here none); before the fix the system's time was read only here
        t := IF ~run THEN base ELSE IF Mutation = "check_then_read" THEN sys ELSE t;
R_trace: \* the trace: the later of the reading and `last` is given, and goes into `last`
        t := Max2(t, last) || last := Max2(last, t);
R_give: \* the reading handed out; one earlier than the thread's last is recorded
        back := back \/ t < gave[self];
        gave[self] := t;
        n := n + 1;
        \* whether the clock ran is read no more: forgotten until the next reading looks
        run := FALSE;
    end while;
end process;

fair process Pauser = -2
variables now = 0, seen = 0;
begin
P_first: \* `Clock::pause`
    if Mutation = "pause_reads_first" then
        \* MUTATION pause_reads_first: the time it stops at read first, as before the fix, the
        \* system's (the clock unmarked, the reading leaves no trace)
        now := sys;
P_mark_late: \* and only then the mark
        altered := TRUE;
        goto P_base;
    else
        \* the mark first, a fence after it
        altered := TRUE;
    end if;
P_read: \* then the time it stops at, a reading of the marked running clock, which leaves its trace
    now := Max2(sys, last) || last := Max2(last, sys);
P_base: \* the paused time stored
    base := now;
P_paused: \* the clock says it is paused
    paused := TRUE;
P_load: \* `last` read: a reading that left its trace so far may be later than the paused time
    seen := last;
P_catch: \* the paused time raised to it
    base := Max2(base, seen);
P_last: \* and `last` raised to the paused time
    last := Max2(last, now);
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
