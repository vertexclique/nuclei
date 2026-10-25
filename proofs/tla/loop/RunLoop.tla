------------------------------ MODULE RunLoop ------------------------------
(***************************************************************************)
(* How often a shard's run loop looks at its IO driver                     *)
(* (src/rt/shard.rs: `Shard::block_on`), one turn of the loop per step.    *)
(*                                                                          *)
(* In plain words: each turn the loop polls its future if it was woken,     *)
(* then runs one task if one is ready. Every `N` task runs it looks at the  *)
(* driver without waiting. A turn with no task in which the future woke     *)
(* itself again counts too, and every `N` such turns the driver gets a      *)
(* look as well: without that, a future that keeps waking itself (a loop    *)
(* of yields) never lets the driver run, and IO it waits on (on a ring, an *)
(* operation not even submitted) never completes. A turn with nothing to    *)
(* run and the future not woken spins and parks, and both look.             *)
(*                                                                          *)
(* What is modelled: the three kinds of turn, in any order the environment  *)
(* picks, and the two counts (`since_drive` and the turns the future woke   *)
(* itself). What is abstract: what a task or the future does, and the look  *)
(* itself (one step, which completes all the IO that is ready).            *)
(***************************************************************************)
EXTENDS Integers

CONSTANTS
    N,         \* DRIVER_INTERVAL
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm RunLoop
variables
    since = 0,              \* task runs since the last look (`since_drive`)
    selfWoken = 0,          \* turns in which no task ran and the future woke itself
    gap = 0,                \* turns since the last look
    lookedSelfWoken = FALSE; \* record: a look came from the future's turns

define
    \* The driver gets a look at least once every 2N - 1 turns.
    BoundedGap == gap < 2 * N
    \* Witnesses.
    NoSelfWokenLook == ~lookedSelfWoken
    GapBelowItsBound == gap < 2 * N - 2
end define;

fair process shard = 1
begin
Turn:
    while TRUE do
        either
            \* A task runs.
            if since + 1 >= N then
                since := 0;
                gap := 0;
            else
                since := since + 1;
                gap := gap + 1;
            end if;
        or
            \* No task ran, and the future woke itself again.
            if Mutation = "no_self_woken_count" then
                gap := gap + 1;
            elsif selfWoken + 1 >= N then
                selfWoken := 0;
                since := 0;
                gap := 0;
                lookedSelfWoken := TRUE;
            else
                selfWoken := selfWoken + 1;
                gap := gap + 1;
            end if;
        or
            \* Nothing to run, the future not woken: the spin and the park look.
            since := 0;
            gap := 0;
        end either;
    end while;
end process;
end algorithm; *)
=============================================================================
