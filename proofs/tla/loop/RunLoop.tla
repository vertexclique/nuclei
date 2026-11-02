------------------------------ MODULE RunLoop ------------------------------
(***************************************************************************)
(* How often a shard's run loop looks at its IO driver                     *)
(* (src/rt/shard/run.rs: `Shard::run_loop`), one turn of the loop per step; *)
(* and how often a turn driven by hand does (`Shard::turn`, `run_ready`,   *)
(* which `Proactor::wait` calls).                                          *)
(*                                                                          *)
(* In plain words: each turn the loop polls its future if it was woken,     *)
(* then runs one task if one is ready. Every `N` task runs it looks at the  *)
(* driver without waiting. A turn with no task in which the future woke     *)
(* itself again counts too, and every `N` such turns the driver gets a      *)
(* look as well: without that, a future that keeps waking itself (a loop    *)
(* of yields) never lets the driver run, and IO it waits on (on a ring, an *)
(* operation not even submitted) never completes. A turn with nothing to    *)
(* run and the future not woken spins and parks, and both look. A future   *)
(* that woke itself in its poll (a yield) lets the tasks queued at that    *)
(* moment run before it is polled again: each of those turns runs a task  *)
(* and counts as one, so the counts below are the same. The spin looks     *)
(* first and starts its clock after that look failed (`Spin::run`,         *)
(* src/rt/shard/spin.rs): the model sees the spin as the look, whatever    *)
(* order it makes its looks in.                                            *)
(*                                                                          *)
(* A turn driven by hand (`Mode = "turn"`) runs the tasks ready, at most N  *)
(* of them, looks at the timers and the poller, runs the tasks ready again, *)
(* at most N, and returns; its caller calls it again. Without the bound, a *)
(* task that yields again and again (waiting for a flag a timer's task     *)
(* sets) keeps the first run of tasks going for good: the timers are never *)
(* fired and the turn never returns. Between two looks at most 2N tasks    *)
(* run: the end of one turn and the start of the next.                     *)
(*                                                                          *)
(* What is modelled: the three kinds of turn, in any order the environment  *)
(* picks, and the two counts (`Shard::since_drive`, which the code keeps   *)
(* as a count down to the next run that weighs a look, and the turns the    *)
(* future woke itself). What is abstract: what a task or the future does,  *)
(* and the look itself (one step, which completes all the IO that is       *)
(* ready).                                                                  *)
(***************************************************************************)
EXTENDS Integers

CONSTANTS
    N,         \* DRIVER_INTERVAL
    Mode,      \* "loop": the run loop; "turn": turns driven by hand
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm RunLoop
variables
    since = 0,              \* task runs since the last look (`Shard::since_drive`)
    selfWoken = 0,          \* turns in which no task ran and the future woke itself
    gap = 0,                \* turns since the last look
    lookedSelfWoken = FALSE, \* record: a look came from the future's turns
    runs = 0;               \* (turn) tasks the current `run_ready` ran

define
    \* The driver gets a look at least once every 2N - 1 turns.
    BoundedGap == gap < 2 * N
    \* Witnesses.
    NoSelfWokenLook == ~lookedSelfWoken
    GapBelowItsBound == gap < 2 * N - 2
    \* (turn) Between two looks of turns driven by hand, at most 2N tasks run.
    TurnGap == gap <= 2 * N
    \* Witness: that bound is reached.
    TurnGapBelowItsBound == gap < 2 * N
end define;

fair process shard = 1
begin
Turn:
    while TRUE do
        if Mode = "turn" then
T_ready:
            \* `run_ready`: a task ready runs, while fewer than N ran, or the tasks ready end.
            \* MUTATION turn_unbounded: it runs every task ready, with no bound.
            while runs < N \/ Mutation = "turn_unbounded" do
                either
                    gap := gap + 1;
                    runs := IF runs < N THEN runs + 1 ELSE runs;
                or
                    goto T_look;
                end either;
            end while;
T_look:
            \* The timers fire and the poller is looked at (waited on, if nothing is ready).
            gap := 0;
            runs := 0;
T_again:
            \* `run_ready` again: what the IO and the timers woke, at most N.
            while runs < N \/ Mutation = "turn_unbounded" do
                either
                    gap := gap + 1;
                    runs := IF runs < N THEN runs + 1 ELSE runs;
                or
                    goto T_return;
                end either;
            end while;
T_return:
            \* The turn returns, and its caller calls the next one.
            runs := 0;
        else
L_turn:
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
        end if;
    end while;
end process;
end algorithm; *)
=============================================================================
