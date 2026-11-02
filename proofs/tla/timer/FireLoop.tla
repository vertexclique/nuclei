------------------------------- MODULE FireLoop -------------------------------
(***************************************************************************)
(* A shard firing the timers that came due, one of whose wakers panics     *)
(* (src/rt/shard/timers.rs: `fire_timers`, `fire_armed_timers`,            *)
(* `fire_precise`, `fire_timers_now`, and the helper they wake the fired   *)
(* wakers through; src/rt/caught.rs: `catch`, `lost`).                     *)
(*                                                                          *)
(* In plain words: a pass over the wheel takes every timer that came due   *)
(* out of it at once, then calls their wakers one after another. A waker   *)
(* is user code, and one may panic. The entries are gone from the wheel    *)
(* already, so a waker the pass does not call is never called: a guard    *)
(* calls the rest. Where the panic goes then: a pass run by `time::advance` *)
(* inside a task hands it to that task, its caller; a pass run by the      *)
(* shard's own loop has no caller, and a panic out of a worker's loop      *)
(* would end its thread and lose its shard, so it is caught there and      *)
(* logged, and the loop runs on.                                           *)
(*                                                                          *)
(* What must never happen: a timer that came due whose waker works and is  *)
(* never called; a shard that stops running because a waker panicked.     *)
(*                                                                          *)
(* What is abstract: the wheel is the set of timers that came due; the     *)
(* shard's further work is one step.                                       *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Due,       \* how many timers came due in one pass
    PanicsAt,  \* the timer whose waker panics (0: none does)
    InTask,    \* the pass is `time::advance`'s, inside a task; else the shard's loop's
    Mutation   \* "none", or the name of the one rule this run takes away

Timers == 1..Due

(* --algorithm FireLoop
variables
    woken = [t \in Timers |-> FALSE],   \* the timer's waker was called and worked
    shardAlive = TRUE,                  \* the shard's thread still runs its loop
    taskPanicked = FALSE,               \* the task that ran `time::advance` got the panic
    laterRan = FALSE;                   \* the shard ran its next work after the pass

define
    \* Every timer that came due with a working waker is woken.
    DueWoken == <>(\A t \in Timers \ {PanicsAt} : woken[t])
    \* The shard runs on after the pass.
    ShardRunsOn == <>laterRan
    \* A pass inside a task hands a panic to that task.
    PanicReachesTask == (InTask /\ PanicsAt # 0 /\ laterRan) => taskPanicked
end define;

fair process shard = "shard"
variables at = 1, panicked = FALSE;
begin
F_wake:
    \* The fired wakers, one after another. The one at PanicsAt panics; the guard calls the rest.
    \* MUTATION drop_on_panic: no guard; the rest are dropped uncalled.
    while at <= Due do
        if at = PanicsAt then
            panicked := TRUE;
            if Mutation = "drop_on_panic" then
                at := Due + 1;
            else
                at := at + 1;
            end if;
        else
            woken[at] := TRUE;
            at := at + 1;
        end if;
    end while;
F_after:
    \* The panic goes on: to the task inside `time::advance`, or, in the shard's own loop, it is
    \* caught and logged. MUTATION run_loop_uncaught: the loop lets it out, and the worker's
    \* thread ends with its shard.
    if panicked then
        if InTask then
            taskPanicked := TRUE;
        elsif Mutation = "run_loop_uncaught" then
            shardAlive := FALSE;
        end if;
    end if;
F_later:
    \* The shard's next work.
    if shardAlive then
        laterRan := TRUE;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
