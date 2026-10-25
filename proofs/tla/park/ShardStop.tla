----------------------------- MODULE ShardStop -----------------------------
(***************************************************************************)
(* A shard that stops, and the runnables pushed to it meanwhile           *)
(* (src/rt/shard.rs: `Shared::push_remote`, `Drop for Shard`), at the     *)
(* grain of the queues' pushes and pops and the flag's accesses.          *)
(*                                                                          *)
(* In plain words: other threads push runnables of `Send` tasks into the  *)
(* shard's inbox. The shard stops: it raises `stopped`, then drops what is *)
(* queued, and the wakers its timers hold, whose tasks come back to it as *)
(* runnables of tasks bound to its thread, and drops those too, pass after *)
(* pass until a pass drops nothing. A pusher looks at `stopped` after its  *)
(* push; seeing it, it drops what the inbox holds itself. Every access is *)
(* sequentially consistent, so either the shard's drain finds a runnable, *)
(* or its pusher sees the flag. Nothing may be left behind: a runnable    *)
(* left in a stopped shard's queue keeps its task, and through its        *)
(* schedule the shard, for good.                                           *)
(*                                                                          *)
(* What is abstract: the queues are counts, and a dropped timer's waker   *)
(* wakes one bound task (which is how a task's going wakes another, a     *)
(* channel's waiter, too).                                                 *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Pushers,   \* the threads that push one runnable each, numbered 1, 2, ...
    Timers,    \* the timers in the shard's wheel as it stops
    Mutation   \* "none", or the name of the one rule this run takes away

ShardT == 0

(* --algorithm ShardStop
variables
    inbox = 0,          \* runnables of `Send` tasks queued
    local = 0,          \* runnables of tasks bound to the shard's thread queued
    stopped = FALSE,
    timers = Timers,    \* timers whose wakers the wheel still holds
    dropped = 0,        \* (record) runnables dropped
    pusherDrained = FALSE, \* (record) a pusher dropped what the inbox held
    passes = 0;         \* (record) the shard's drain passes

define
    AllDone == \A p \in Pushers \cup {ShardT} : pc[p] = "Done"
end define;

fair process shard = ShardT
variables quiet = FALSE;
begin
S_stop:
    stopped := TRUE;
S_pass:
    while ~quiet do
        \* The wakers of its timers go: each wakes a bound task, queued back here.
        \* MUTATION one_pass: they go after the one drain, as the shard's fields do.
        quiet := timers = 0;
        if Mutation # "one_pass" then
            local := local + timers;
            timers := 0;
        end if;
        passes := passes + 1;
S_drain:
        while inbox + local > 0 do
            if local > 0 then
                local := local - 1;
            else
                inbox := inbox - 1;
            end if;
            dropped := dropped + 1;
            quiet := FALSE;
        end while;
        if Mutation = "one_pass" then
            local := local + timers;
            timers := 0;
            quiet := TRUE;
        end if;
    end while;
end process;

fair process pusher \in Pushers
begin
P_first:
    \* MUTATION check_first: the pusher looks at the flag before it pushes.
    if Mutation = "check_first" then
        if stopped then
            goto Done;
        end if;
    end if;
P_push:
    inbox := inbox + 1;
    if Mutation = "check_first" then
        goto Done;
    end if;
P_look:
    \* MUTATION no_pusher_drop: the pusher leaves its runnable to the shard.
    if stopped /\ Mutation # "no_pusher_drop" then
        goto P_drop;
    else
        goto Done;
    end if;
P_drop:
    while inbox > 0 do
        inbox := inbox - 1;
        dropped := dropped + 1;
        pusherDrained := TRUE;
    end while;
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

\* Once the shard and every pusher are done, no runnable is left behind.
NothingLeft == AllDone => (inbox = 0 /\ local = 0)

\* Nothing is dropped twice or made up.
DroppedOnce == dropped <= Cardinality(Pushers) + Timers

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A pusher sees the flag and drops what the inbox holds.
NoPusherDrain == ~pusherDrained
\* The shard drains more than once: what went with a task woke another.
NoSecondPass == passes < 2
=============================================================================
