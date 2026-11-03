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
(* src/rt/shard/spin.rs), and that first look asks the poller before it    *)
(* looks at anything else: the model sees a spin that found work as a      *)
(* look.                                                                   *)
(*                                                                          *)
(* A turn driven by hand (`Mode = "turn"`) runs the tasks ready, at most N  *)
(* of them, looks at the timers and the poller, runs the tasks ready again, *)
(* at most N, and returns; its caller calls it again. Without the bound, a *)
(* task that yields again and again (waiting for a flag a timer's task     *)
(* sets) keeps the first run of tasks going for good: the timers are never *)
(* fired and the turn never returns. Between two looks at most 2N tasks    *)
(* run: the end of one turn and the start of the next.                     *)
(*                                                                          *)
(* An idle pass may look at no driver at all: the park backs out (work    *)
(* came after the last look) with the spin cold. Such a pass keeps the     *)
(* count of runs where it was, and counts like a turn in which the future  *)
(* woke itself: every `N` of them bring a look without waiting. Before, it *)
(* started the count over: work posted at that moment, again and again,   *)
(* kept the poller from ever being looked at.                              *)
(*                                                                         *)
(* A warm spin may find the future woken from another thread just after    *)
(* the pass looked at it (`SpinWoken`). The spin's first look asks the     *)
(* poller before it looks at the future again (src/rt/shard/run.rs,        *)
(* `run_loop`), so a spin that finds work has looked, and the count starts *)
(* over rightly. Before, the first look read the future's flag first and   *)
(* stopped there: the count started over with no look, and a future woken  *)
(* in that window, again and again, kept the poller from being looked at   *)
(* (`M_spin_woken_unlooked`).                                              *)
(*                                                                          *)
(* What is modelled: the four kinds of turn, in any order the environment   *)
(* picks, and the two counts (`Shard::since_drive`, which the code keeps   *)
(* as a count down to the next run that weighs a look, and the turns the    *)
(* future woke itself, which idle passes with no look share). What is      *)
(* abstract: what a task or the future does, and the look itself (one      *)
(* step, which completes all the IO that is ready).                        *)
(***************************************************************************)
EXTENDS Integers

CONSTANTS
    N,         \* DRIVER_INTERVAL
    BackOut,   \* an idle pass may look at no driver: a park that backs out with the spin cold
    SpinWoken, \* a warm spin may find the future woken from another thread, after the pass looked at it
    Mode,      \* "loop": the run loop; "turn": turns driven by hand
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm RunLoop
variables
    since = 0,              \* task runs since the last look (`Shard::since_drive`)
    selfWoken = 0,          \* turns in which no task ran and the future woke itself
    gap = 0,                \* turns since the last look
    lookedSelfWoken = FALSE, \* record: a look came from the future's turns
    lookedUnlooked = FALSE, \* record: a look came from the idle passes that looked at no driver
    runs = 0;               \* (turn) tasks the current `run_ready` ran

define
    \* The driver gets a look at least once every 2N - 1 turns.
    BoundedGap == gap < 2 * N
    \* Witnesses.
    NoSelfWokenLook == ~lookedSelfWoken
    \* Witness: idle passes that looked at no driver bring a look of their own.
    NoUnlookedLook == ~lookedUnlooked
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
            \* MUTATION no_run_count: a task's run is not counted (`drive_left` not counted down).
            if Mutation = "no_run_count" then
                gap := gap + 1;                                 \* a turn without a look
            elsif since + 1 >= N then
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
            \* Nothing to run, the future not woken, and the pass looks: the park parks (its
            \* drive waits on the poller), or the spin asked the poller (it found work, or none
            \* and the park backed out after it). The count starts over (`count_again`).
            since := 0;
            gap := 0;
        or
            \* Nothing to run, the future not woken, and the pass looks at no driver: the park
            \* backs out (`begin_park` found work posted after the last look) with the spin
            \* cold (src/rt/shard/run.rs, `run_loop`, its idle pass).
            await BackOut;
            if Mutation = "backout_resets" then
                \* MUTATION backout_resets: counted like the future's own turn, but the count of
                \* runs started over, as the code did before: runs and such passes take turns,
                \* each count kept below its bound, and the driver waits for good.
                since := 0;                                     \* the count of runs started over
                if selfWoken + 1 >= N then                      \* the Nth such pass in all:
                    selfWoken := 0;                             \* a look without waiting
                    gap := 0;                                   \* the driver was looked at
                else                                            \* else counted only
                    selfWoken := selfWoken + 1;                 \* one more
                    gap := gap + 1;                             \* a turn without a look
                end if;
            elsif Mutation = "backout_uncounted" then
                \* MUTATION backout_uncounted: the count of runs kept, but the pass not counted:
                \* such passes, again and again, never bring a look.
                gap := gap + 1;                                 \* a turn without a look
            elsif selfWoken + 1 >= N then
                \* The fix: the count of runs kept where it was, and every Nth such pass (the
                \* future's own turns counted with them) looks without waiting.
                selfWoken := 0;                                 \* that count starts over
                since := 0;                                     \* the look starts the count of runs over
                gap := 0;                                       \* the driver was looked at
                lookedUnlooked := TRUE;                         \* (record) a look from such passes
            else
                \* The fix, below the Nth: the count of runs kept, the pass counted.
                selfWoken := selfWoken + 1;                     \* one more
                gap := gap + 1;                                 \* a turn without a look
            end if;
        or
            \* Nothing to run, and a warm spin finds the future woken from another thread after
            \* the pass looked at it (src/rt/shard/run.rs, `run_loop`, the spin's looks).
            await SpinWoken;                                    \* only where such a wake is checked
            if Mutation = "spin_woken_unlooked" then
                \* MUTATION spin_woken_unlooked: the spin's first look reads the future's flag
                \* before it asks the poller, as the code did before, and stops there.
                since := 0;                                     \* the count of runs started over
                gap := gap + 1;                                 \* a turn without a look
            else
                \* The fix: the first look asks the poller before it looks at the future, so
                \* the spin that finds the wake has looked.
                since := 0;                                     \* the look starts the count of runs over
                gap := 0;                                       \* the driver was looked at
            end if;
        end either;
        end if;
    end while;
end process;
end algorithm; *)
=============================================================================
