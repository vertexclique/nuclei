------------------------------- MODULE Nested -------------------------------
(***************************************************************************)
(* Which runtime the code on a thread sees as current, and whose timer    *)
(* wheels are in place, while one runtime's `block_on` runs nested on the *)
(* shard of another (src/rt/runtime.rs: `block_on_here`, `current`,        *)
(* `with_clock`, `spawn`; src/rt/shard/foreign.rs: `Shard::block_on_nested`,*)
(* `Shard::serving`, `Shard::block_on_foreign`, `poll_inside`, `Aside`,    *)
(* `side_for`, `Frame`, `Visiting`; src/rt/runtime.rs: `spawn_unworked`;  *)
(* src/rt/context.rs: `set_runtime`, `RuntimeSet`;                        *)
(* src/rt/runtime/enter.rs: `Runtime::enter`, `worker_index`).             *)
(*                                                                          *)
(* Who acts. One thread T, running shard S. S serves runtime L: S is a      *)
(* worker of L, or S is T's own shard and L is the runtime whose outermost *)
(* `block_on` runs it (the shard's visiting mark). T's thread-local        *)
(* "current runtime" (`RUNTIME`) is what every spawn, clock read, blocking *)
(* job, handle lookup and worker index of the code running on T reads. S   *)
(* has one set of timer wheels in place, L's own (set 0) unless a nested   *)
(* future of another runtime is being polled.                              *)
(*                                                                          *)
(* What can happen. S's loop polls its tasks, and the future of the        *)
(* innermost `block_on` running on S. The code a poll runs may arm a       *)
(* timer (in the wheels in place), call `X.enter()` (X current until its  *)
(* guard drops), call `X.block_on(f)` (a frame nested on S), return, or    *)
(* panic. A nested frame first puts aside the wheels of a future whose     *)
(* poll it was called from, so L's are in place for its loop; if X is not  *)
(* L the frame is foreign: its loop runs with L current, and each poll of  *)
(* f with X current and X's wheels in place (the wheels of an earlier      *)
(* frame of X if there is one, else a set of its own), both put back after *)
(* the poll. A panic unwinds element by element, each guard putting back   *)
(* what it found, until a task's poll catches it; with none, it leaves the *)
(* thread.                                                                 *)
(*                                                                          *)
(* What must never happen. S's loop runs, or polls one of S's tasks, with *)
(* a runtime other than L current, or with wheels other than L's in place  *)
(* (a task's spawns would go to another runtime, its clock reads read     *)
(* another clock, its sleeps wait in another runtime's wheels); a nested  *)
(* frame's future is polled with a runtime other than its own current, or *)
(* with other wheels in place; a timer armed by code of X is dropped while *)
(* a frame of X is still on S (its sleep would read as elapsed).           *)
(*                                                                          *)
(* A task of S is L's, or a visitor of X: spawned by X's code on S       *)
(* because X has no worker (`spawn_unworked`), it runs wrapped so each     *)
(* poll, and its drop, has X current (`Visiting`); its sleeps wait in the  *)
(* wheels in place, L's (a limit: a witness says so).                      *)
(*                                                                          *)
(* What is abstract. One shard; a task is one poll of code; how a set of   *)
(* wheels fires                                                            *)
(* (tla/clock/OneWheel.tla); how `Shard::serving` finds L (a worker's      *)
(* runtime, or the own shard's visiting mark).                             *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Runtimes,   \* the runtimes the code may enter or block on
    L,          \* the runtime S serves: the one its tasks and its loop see
    MaxStack,   \* the deepest the stack of frames, polls and guards may grow
    Mutation    \* "none", or the name of the one rule this run takes away

\* What an element's guard puts back as it is popped, from current runtime c: a frame's guards
\* the runtime its call found, an enter guard the one it found, a future poll's guard the loop's
\* runtime (MUTATION no_restore: none), a task's poll the runtime its visitor's wrapper found (an L
\* task's poll found L, and sets nothing).
Restore(c, e) ==                                                             \* the runtime after e's guard
    CASE e.k = "fut" -> IF Mutation = "no_restore" THEN c ELSE e.saved       \* `poll_inside`'s guard
      [] e.k \in {"bo", "en", "task"} -> e.saved                             \* `RuntimeSet`'s drop, `Visiting`'s
      [] OTHER -> c                                                          \* nothing else sets it

(* --algorithm Nested                                                        \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    cur = L,                 \* the thread's current runtime (`RUNTIME`): L, as S's loop starts
    frames = << [k |-> "bo", rt |-> L, foreign |-> FALSE, saved |-> L,     \* the stack: S's own loop at the bottom
                 ws |-> 0, pushed |-> FALSE, held |-> 0] >>,
    inPlace = 0,             \* the set of wheels in place: 0 is L's own, n > 0 the one the frame at n made
    armed = {},              \* the sets that hold a timer armed in them
    lost = FALSE,            \* a timer was dropped with its set while a frame of its runtime stayed on S
    gone = FALSE,            \* a panic unwound out of the bottom: the thread left
    unwoundForeign = FALSE,  \* (record) an unwind went through a foreign frame and a task caught it
    reused = FALSE;          \* (record) a foreign frame polled its future with an earlier frame's set

define                                                                       \* names the steps and properties use:
    \* The element at the top of the stack: what runs now.
    Top == frames[Len(frames)]                                               \* the innermost element
    \* The stack without its top.
    Below == SubSeq(frames, 1, Len(frames) - 1)                              \* the rest
    \* S's loop runs now: the top is a frame, between polls.
    InLoop == Top.k = "bo"                                                   \* a frame's loop
    \* The runtime of the set of wheels n: L's own, or that of the frame at n, which made it.
    SetRt(n) == IF n = 0 THEN L ELSE frames[n].rt                            \* set 0 is L's own
    \* S's loop never runs with a runtime other than L current.
    LoopSeesOwn == (~gone /\ InLoop) => cur = L                              \* the loop is L's
    \* A task of S polled now sees its own runtime: L, or the runtime a visitor came from.
    TaskSeesOwn == (~gone /\ Top.k = "task") => cur = Top.rt                 \* the task's own
    \* A nested frame's future polled now sees its frame's runtime.
    FutureSeesOwn == (~gone /\ Top.k = "fut") => cur = frames[Len(frames) - 1].rt  \* the future is its frame's
    \* Code inside an enter guard sees the runtime it entered.
    EnterSeesIt == (~gone /\ Top.k = "en") => cur = Top.rt                   \* what the user asked for
    \* S's loop, and L's tasks, run with L's wheels in place.
    LoopWheelsOwn == (~gone /\ (InLoop \/ (Top.k = "task" /\ Top.rt = L))) => SetRt(inPlace) = L  \* L's in L's
    \* The frames of runtime x nested on S that hold wheels of x.
    FramesOf(x) == {j \in 1..Len(frames) : frames[j].k = "bo" /\ frames[j].foreign /\ frames[j].rt = x}  \* x's sets
    \* (The limit, a witness) a visitor of x is polled with x's wheels in place while a frame of x holds them.
    VisitorWheelsOwn == (~gone /\ Top.k = "task" /\ Top.rt # L /\ FramesOf(Top.rt) # {})  \* such a visitor:
        => SetRt(inPlace) = Top.rt                                           \* its runtime's wheels
    \* A nested frame's future is polled with its own runtime's wheels in place.
    FutureWheelsOwn == (~gone /\ Top.k = "fut") => SetRt(inPlace) = frames[Len(frames) - 1].rt  \* its own
    \* No timer is dropped with its set while a frame of its runtime is still on S.
    NoTimerLost == ~lost                                                     \* kept while it can fire
    \* Witness: a foreign frame runs nested inside another foreign frame.
    NoForeignInForeign == ~\E i, j \in 1..Len(frames) :                      \* claims to: never
        i < j /\ frames[i].k = "bo" /\ frames[j].k = "bo" /\ frames[i].foreign /\ frames[j].foreign
    \* Witness: an unwind goes through a foreign frame and a task's poll below catches it.
    NoUnwoundForeign == ~unwoundForeign                                      \* claims to: never
    \* Witness: a foreign frame polls its future with the set an earlier frame of its runtime made.
    NoReuse == ~reused                                                       \* claims to: never
end define;                                                                  \* end of the names

\* A frame that made set n goes: the set goes with its timers; one armed is lost if a frame of its
\* runtime is still among the elements of rest.
macro DropSet(n, made, rest) begin                                           \* `Frame`'s drop
    if made then                                                             \* the frame made the set
        lost := lost \/ (n \in armed /\                                      \* an armed timer goes with it
                         \E j \in 1..Len(rest) : rest[j].k = "bo" /\ rest[j].rt = SetRt(n));  \* its runtime still nested
        armed := armed \ {n};                                                \* the set is gone
    end if;                                                                  \* else an earlier frame's: kept
end macro;                                                                   \* end of the macro

fair process thread = "T"                                                    \* the one thread
begin                                                                        \* its steps:
Step:                                                                        \* one thing happens
    while ~gone do                                                           \* until a panic leaves the thread
        either                                                               \* S's loop polls one of its tasks
            await InLoop /\ Len(frames) < MaxStack;                          \* between polls, room left
            with x \in Runtimes do                                           \* L's task, or a visitor of x
                frames := Append(frames, [k |-> "task", rt |-> x, saved |-> cur, held |-> inPlace]);  \* guards keep both
                \* MUTATION visitor_unwrapped: a visitor polled with no wrapper keeps the loop's runtime.
                cur := IF x # L /\ Mutation # "visitor_unwrapped" THEN x ELSE cur;  \* `Visiting`: x current
            end with;                                                        \* the task's code runs now, wheels as they are
        or                                                                   \* S's loop polls its frame's future
            await InLoop /\ Len(frames) < MaxStack;                          \* between polls, room left
            \* MUTATION future_under_loop: the future's poll is not switched to its runtime.
            with f = Top, switch = Top.foreign /\ Mutation # "future_under_loop" do  \* `poll_inside` of the frame f
                frames := Append(frames, [k |-> "fut", saved |-> cur, held |-> inPlace]);  \* its guards keep both
                cur := IF switch THEN f.rt ELSE cur;                         \* `set_runtime(inner)` for the poll
                inPlace := IF f.foreign THEN f.ws ELSE inPlace;              \* `Swapped`: the frame's wheels in place
            end with;                                                        \* the future's code runs now
        or                                                                   \* the code at the top arms a timer
            await Top.k \in {"task", "fut"};                                 \* a poll's code runs
            armed := armed \cup {inPlace};                                   \* in the wheels in place
        or                                                                   \* the code at the top returns
            await ~InLoop /\ Len(frames) > 1;                                \* a poll or an enter guard
            \* A task's poll ends, a future's poll is pending (its guards put the loop's runtime and
            \* wheels back; MUTATION no_restore: not the runtime), or an enter guard drops.
            cur := Restore(cur, Top);                                        \* that element's guard
            inPlace := IF Top.k \in {"fut", "task"} THEN Top.held ELSE inPlace;  \* `Swapped`'s drop
            frames := Below;                                                 \* gone from the stack
        or                                                                   \* a nested frame's future completes
            await Top.k = "fut" /\ Len(frames) > 2;                          \* its poll answers Ready, not the bottom's
            with f = frames[Len(frames) - 1], rest = SubSeq(frames, 1, Len(frames) - 2) do  \* the frame, what stays
                cur := f.saved;                                              \* its guards put back what its call found
                inPlace := f.held;                                           \* and the wheels it found
                DropSet(f.ws, f.pushed, rest);                               \* a set it made goes
                frames := rest;                                              \* the poll and the frame end
            end with;                                                        \* returned
        or                                                                   \* the code calls `X.enter()`
            await ~InLoop /\ Len(frames) < MaxStack;                         \* code runs, room left
            with x \in Runtimes do                                           \* any runtime
                frames := Append(frames, [k |-> "en", rt |-> x, saved |-> cur]);  \* its guard keeps what it found
                cur := x;                                                    \* x is current
            end with;                                                        \* until the guard drops
        or                                                                   \* the code calls `X.block_on(f)`: nested on S
            await ~InLoop /\ Len(frames) < MaxStack;                         \* code runs, room left
            with x \in Runtimes do                                           \* any runtime
                \* `block_on_here`: x current, then the decision. Fixed: foreign when x is not L
                \* (`Shard::serving`). MUTATION previous_decides: foreign when x is not the runtime the
                \* call found, the loop then run under that one (as the first wheel-per-runtime change
                \* had it). MUTATION no_switch: never foreign, the loop under x (as before both).
                with foreign = CASE Mutation = "no_switch" -> FALSE          \* never
                                 [] Mutation = "previous_decides" -> cur # x \* by the runtime found
                                 [] OTHER -> x # L,                          \* by the runtime S serves
                     loopRt = IF Mutation = "previous_decides" THEN cur ELSE L,  \* the loop's runtime
                     \* `side_for`: the set of an earlier foreign frame of x (MUTATION no_reuse: never).
                     earlier = FramesOf(x),                                  \* the frames whose set x may share
                     share = foreign /\ Mutation # "no_reuse" /\ earlier # {} do  \* shared, not made
                    frames := Append(frames,                                 \* the frame, its guards keeping what it found
                        [k |-> "bo", rt |-> x, foreign |-> foreign, saved |-> cur, held |-> inPlace,
                         ws |-> IF ~foreign THEN 0                           \* a plain frame: L's own
                                ELSE IF share THEN frames[CHOOSE j \in earlier : TRUE].ws  \* the earlier one's
                                ELSE Len(frames) + 1,                        \* a new set: this frame's
                         pushed |-> foreign /\ ~share]);                     \* it made its set
                    reused := reused \/ share;                               \* (record) a set shared
                    cur := IF foreign THEN loopRt ELSE x;                    \* `block_on_foreign`: the loop's runtime
                    \* `Aside`: a future's wheels in place go aside for the frame's loop (MUTATION no_aside: not).
                    inPlace := IF Mutation = "no_aside" THEN inPlace ELSE 0; \* L's own in place
                end with;                                                    \* the frame's loop runs now
            end with;                                                        \* nested
        or                                                                   \* the code panics
            await ~InLoop /\ Len(frames) > 1;                                \* code runs
            with caught = {j \in 1..Len(frames) : frames[j].k = "task"} do   \* the polls that would catch it
                if caught = {} then                                          \* none: it leaves the thread
                    gone := TRUE;                                            \* the thread unwinds out
                else                                                         \* the nearest task's poll catches it
                    with i = CHOOSE j \in caught : \A m \in caught : m <= j, \* the innermost
                         rest = SubSeq(frames, 1, i - 1),                    \* what stays
                         made = {frames[m].ws : m \in {m \in (i + 1)..Len(frames) :  \* the sets popped frames made
                                    frames[m].k = "bo" /\ frames[m].pushed}} do
                        unwoundForeign := unwoundForeign \/                  \* (record) through a foreign frame
                            \E m \in (i + 1)..Len(frames) : frames[m].k = "bo" /\ frames[m].foreign;
                        lost := lost \/ \E n \in made : n \in armed /\       \* a set gone with an armed timer
                            \E j \in 1..Len(rest) : rest[j].k = "bo" /\ rest[j].rt = SetRt(n);  \* its runtime nested
                        armed := armed \ made;                               \* those sets are gone
                        \* Every guard above it, top down, then the guards of its own wrapper, inside its catch.
                        cur := frames[i].saved;                              \* what the task's poll found
                        inPlace := frames[i].held;                           \* and the wheels it found
                        frames := rest;                                      \* its poll ends with the panic
                    end with;                                                \* caught
                end if;                                                      \* the unwind is over
            end with;                                                        \* done
        end either;                                                          \* one step
    end while;                                                               \* the next one
end process;                                                                 \* end of the thread
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================
