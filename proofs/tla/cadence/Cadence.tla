------------------------------ MODULE Cadence ------------------------------
(***************************************************************************)
(* How long an IO operation a task queued on a busy shard waits before the *)
(* shard's run loop hands it to the kernel (src/rt/shard/run.rs:           *)
(* `Shard::run_worker`, `Shard::run_loop`, `Shard::drive_overdue`; the     *)
(* clock is src/rt/quartz.rs), one task run per step.                      *)
(*                                                                          *)
(* In plain words: a task has queued an operation (a receive pushed into   *)
(* the ring's submission queue). It reaches the kernel only when the run   *)
(* loop "drives" the shard's IO. The loop drives in three cases. Every N   *)
(* task runs (a count that lives on the shard and moves with it). Every Q  *)
(* runs it reads a coarse clock, and drives if the last drive is older     *)
(* than a bound (the time rule). And a runner that has just taken the      *)
(* shard over, after the monitor handed it on from a task that blocked     *)
(* past the threshold, drives before it runs any task (the first drive).   *)
(* A task takes one of three amounts of time: none worth counting (a       *)
(* quick one), a while but under the handoff threshold (a slow one), or    *)
(* past the threshold (a long one, which gets the shard handed on). The    *)
(* runner that ran a long task is detached: it drives and checks nothing   *)
(* more.                                                                   *)
(*                                                                          *)
(* The count alone bounds the wait in runs, but not in time: N slow runs   *)
(* are N times a few milliseconds. What must hold: the time the operation  *)
(* waits is at most Bound, a few runs' worth, whatever mix of tasks the    *)
(* environment picks. Take away the first drive and a shard handed on      *)
(* again and again is checked only at every Q-th run of long tasks; take   *)
(* away the time rule and slow tasks wait for the count.                   *)
(*                                                                          *)
(* What is modelled: the three kinds of task, the count since the last     *)
(* drive, the time since the last drive, the handoff after a long task, the *)
(* new runner's first drive. What is abstract: time is whole units (a run  *)
(* adds its kind's amount), what a task does, the coarse clock's own       *)
(* resolution (the bound is whole units), the drive itself (one step that  *)
(* hands every queued operation to the kernel).                            *)
(***************************************************************************)
EXTENDS Integers

CONSTANTS
    N,           \* DRIVER_INTERVAL: runs between drives by count
    Q,           \* QUARTZ_EVERY: runs between reads of the coarse clock
    Slow,        \* time of a slow task (under the handoff threshold)
    Long,        \* time of a long task (past it)
    DriveBound,  \* DRIVE_BOUND: time since the last drive that makes a read drive
    Bound,       \* the most time the queued operation may wait
    Mode         \* "ok", or the name of the one rule this run takes away

(* --algorithm Cadence
variables
    since = 0,            \* task runs since the last drive (`since_drive`)
    age = 0,              \* time since the last drive (what `last_drive` and the clock tell)
    waited = 0,           \* time the queued operation has waited so far
    submitted = FALSE,    \* the operation reached the kernel (a drive happened)
    fresh = FALSE,        \* a new runner has taken the shard and has not run yet
    byFirst = FALSE,      \* record: the new runner's first drive submitted it
    byTime = FALSE;       \* record: the time rule submitted it

define
    \* The operation never waits longer than Bound, even after it is submitted
    \* (`waited` stops growing then).
    Bounded == waited <= Bound
    \* Witnesses.
    NoFirstDrive == ~byFirst
    NoTimeDrive == ~byTime
    WaitBelowWorst == waited < (Q - 1) * Slow + Long
end define;

fair process shard = 1
begin
Turn:
    while ~submitted do
        if fresh then
            \* The new runner starts: it drives once before any task.
            fresh := FALSE;
            if Mode /= "no_first_drive" then
                submitted := TRUE;
                since := 0;
                age := 0;
                byFirst := TRUE;
            end if;
        else
            \* One task runs; the environment picks which kind.
            with c \in {0, Slow, Long} do
                waited := waited + c;
                if c = Long then
                    \* Past the threshold: the shard is handed on, this runner
                    \* is detached and drives and checks nothing more.
                    fresh := TRUE;
                    since := since + 1;
                    age := age + c;
                elsif since + 1 >= N then
                    \* The count: N runs since the last drive.
                    submitted := TRUE;
                    since := 0;
                    age := 0;
                elsif Mode /= "no_time_rule" /\ (since + 1) % Q = 0 /\ age + c >= DriveBound then
                    \* Every Q-th run reads the clock, and the last drive is old.
                    submitted := TRUE;
                    since := 0;
                    age := 0;
                    byTime := TRUE;
                else
                    since := since + 1;
                    age := age + c;
                end if;
            end with;
        end if;
    end while;
end process;
end algorithm; *)
=============================================================================
