------------------------------ MODULE OneWheel ------------------------------
(***************************************************************************)
(* One timer wheel read through two runtimes' clocks (src/rt/shard/        *)
(* timers.rs: `Shard::wheel_now`, `fire_timers`; src/time/clock.rs:        *)
(* `now_nanos`, which reads the clock of the runtime the thread is inside; *)
(* src/rt/runtime.rs: `block_on_here`, which nests a second runtime's      *)
(* `block_on` on the shard the thread runs, and takes                       *)
(* src/rt/shard/foreign.rs `Shard::block_on_foreign` for another runtime:  *)
(* the loop runs under the outer runtime, the nested future's polls under  *)
(* the inner one with its wheels swapped in; src/rt/shard/timers.rs        *)
(* `fire_precise` and `next_deadline`, which fire the inner wheels between *)
(* those polls by the inner clock).                                        *)
(*                                                                          *)
(* In plain words: a thread's own shard has one wheel, and serves the      *)
(* `block_on` of every runtime the thread calls. Inside runtime A's        *)
(* `block_on`, a sleep arms the wheel for a deadline on A's clock. A task  *)
(* then calls runtime B's `block_on`, which nests on the same shard. B's   *)
(* clock is paused and advanced. The shard looks at its wheel in the       *)
(* nested loop and reads the time from B's clock, the runtime it is inside *)
(* now: past the deadline, so A's sleep fired, though A's clock had not    *)
(* reached it (the `one_wheel` mutation). Each runtime now has a wheel of  *)
(* its own on the shard: B's nested `block_on` swaps B's in, and the fire  *)
(* pass reads A's wheel, kept aside, by A's clock and B's by B's.          *)
(*                                                                          *)
(* What must never happen: a sleep ends before its deadline on its own     *)
(* runtime's clock.                                                        *)
(*                                                                          *)
(* What is abstract: time is a count; A's clock runs (it moves on by one,  *)
(* up to three), B's is paused at the same time A's was at and moves only *)
(* by the advance; the wheel fires at its look whatever is due by the time *)
(* it reads.                                                               *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Advance,   \* how far B's paused clock is advanced inside its `block_on`
    Watched,   \* the shard is a worker's, whose polls the handoff may take; else a thread's own
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm OneWheel
variables
    clockA = 0,          \* runtime A's clock, running
    clockB = 0,          \* runtime B's clock, paused where A's stood
    inside = "A",        \* the runtime whose `block_on` the thread is innermost in
    deadline = 1000,     \* the sleep's deadline on A's clock (none yet)
    fired = FALSE,       \* the sleep's timer fired
    firedAt = 0,         \* A's clock when it fired
    swapped = FALSE,     \* B's wheels are swapped into the shard for the nested poll
    taken = FALSE,       \* the handoff took the shard from the thread during that poll
    touchedAfterTake = FALSE; \* (record) the thread touched the shard's wheels after the take

define
    \* The sleep never ends before its deadline on A's clock.
    NeverEarly == fired => firedAt >= deadline
    \* A thread never touches a shard taken from it.
    NoTouchAfterTake == ~touchedAfterTake
end define;

fair process Time = "time"
begin
T_tick: \* A's clock moves on, one unit at a time, up to three
    while clockA < 3 do
        clockA := clockA + 1;
    end while;
end process;

fair process Thread = "thread"
begin
H_arm: \* inside A's `block_on`, a sleep arms the shard's wheel one unit ahead on A's clock
    deadline := clockA + 1;
H_enter: \* a task calls B's `block_on`, which nests on the same shard; its future's poll swaps
         \* B's wheels in, only on a shard no handoff can take (a thread's own).
         \* MUTATION swap_on_watched: it swaps them in on a worker's shard too.
    inside := "B";
    swapped := ~Watched \/ Mutation = "swap_on_watched";
H_take: \* on a worker's shard, the handoff may take the shard while the nested poll blocks
    if Watched then
        either taken := TRUE; or skip; end either;
    end if;
H_advance: \* B's paused clock is advanced
    clockB := clockB + Advance;
H_look: \* the nested loop's look at A's sleep: on A's wheel, by A's clock. MUTATION one_wheel: one
        \* wheel for both, read by the clock of the runtime the thread is innermost in
    if ~fired /\ (IF inside = "A" \/ Mutation # "one_wheel" THEN clockA ELSE clockB) >= deadline then
        fired := TRUE || firedAt := clockA;
    end if;
H_leave: \* the nested poll ends: B's wheels are swapped back out, only if the shard is still this
         \* thread's (a taken one is touched no more); then B's `block_on` returns
    if swapped then
        touchedAfterTake := taken /\ Mutation = "swap_on_watched";
        swapped := FALSE;
    end if;
    inside := "A";
H_wait: \* back in A's loop, its looks fire the timer once A's clock reaches the deadline
    await fired \/ clockA >= deadline;
    if ~fired then fired := TRUE || firedAt := clockA end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
