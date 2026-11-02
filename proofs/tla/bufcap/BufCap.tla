------------------------------ MODULE BufCap ------------------------------
(***************************************************************************)
(* What the IO buffers of a thread that runs no shard may hold             *)
(* (src/buf/mod.rs: `with_pool`, the `FALLBACK` pool made on first need,   *)
(* `serve_with_cap` and `Pool::hold_to`; `Pool::charge` and `make_room`;   *)
(* the threads that serve a runtime call `serve_with_cap` as they start:   *)
(* `Pool::work` in src/rt/blocking.rs, `runner` in src/rt/workers.rs, and  *)
(* `enter` in src/rt/runtime/enter.rs).                                    *)
(*                                                                          *)
(* In plain words: a runtime's workers each have a pool of IO buffers that *)
(* holds at most the runtime's `io_buffer_cap`. A thread that runs no      *)
(* shard (a thread of the runtime's blocking pool, one inside the runtime  *)
(* with `enter`, a worker whose shard was handed over) takes its buffers  *)
(* from a pool of its own, made the first time it needs one. Each pool     *)
(* counts every buffer it took from the allocator, in use or free, and     *)
(* takes none past its cap: at the cap it reuses a free one, else the side *)
(* that asked waits for one to come back. A buffer may be handed to       *)
(* another thread (its read-ahead moved with a handle) and dropped there:  *)
(* it goes back to its own pool's return list, still counted, and the     *)
(* pool takes it back the next time it needs one.                          *)
(*                                                                          *)
(* A thread that serves a runtime holds its pool to the runtime's cap as   *)
(* it starts serving: a pool thread and a worker before their first job,   *)
(* so the pool is fresh and holds nothing past the cap, ever. Before the  *)
(* fix the pool kept the 64 MiB default whatever the runtime said, so a    *)
(* runtime built to hold 1 MiB of IO buffers per thread held 64 MiB on     *)
(* each of its pool threads. A thread that held its pool's buffers before  *)
(* it began serving (`enter` after IO of its own: `MB_late_cap`) frees its *)
(* idle buffers past the cap at once and takes no new memory until it is  *)
(* back under it; a thread that serves no runtime keeps the default.       *)
(*                                                                          *)
(* What must never happen: a pool of a thread that serves the runtime     *)
(* holds more than the runtime's cap (`PerPool`); all of them together     *)
(* hold more than the cap times the pools (`Total`).                       *)
(*                                                                          *)
(* What is abstract: buffers are of one size, and the cap and the default *)
(* are counted in buffers; a thread takes and gives back one buffer a     *)
(* step; the worker's pool is one more pool with the runtime's cap, as     *)
(* every worker's is (`Shard::new`).                                       *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC                                            \* numbers, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Cap,       \* the runtime's `io_buffer_cap`, in buffers
    Default,   \* the default a pool takes with no runtime to serve, in buffers (more than `Cap`)
    Threads,   \* threads that serve the runtime and run no shard
    Takes,     \* buffers each thread asks for, one at a time, over its life
    Mutation   \* "none", or the name of the one rule this run takes away

Workers == {0}                                                               \* the runtime's worker, whose pool is its shard's
Others == 1..Threads                                                         \* the threads that run no shard
Pools == Workers \cup Others                                                 \* every pool, one per thread
Min(a, b) == IF a < b THEN a ELSE b                                          \* the smaller of two numbers
Max(a, b) == IF a > b THEN a ELSE b                                          \* the larger of two numbers
Sum(f, S) == LET F[T \in SUBSET S] == IF T = {} THEN 0 ELSE LET x == CHOOSE y \in T : TRUE IN f[x] + F[T \ {x}] IN F[S] \* the sum of f over S

(* --algorithm BufCap
variables
    served = [t \in Pools |-> t \in Workers],  \* the thread said which runtime it serves (a worker always has)
    made = [t \in Pools |-> IF t \in Workers THEN Cap ELSE 0], \* the cap its pool got when made; 0: not made yet
    charged = [t \in Pools |-> 0],             \* buffers its pool holds, in use, free or on their way back
    inUse = [t \in Pools |-> 0],               \* of those, in use by the thread's sides
    away = [t \in Pools |-> 0],                \* of those, handed to another thread, not dropped yet
    back = [t \in Pools |-> 0],                \* of those, dropped elsewhere: on the pool's return list
    asked = [t \in Pools |-> 0];               \* buffers the thread asked for so far

\* Each thread, its shard's or none: says which runtime it serves, then asks for buffers and gives
\* them back, or hands them to another thread.
define
    \* Buffers of the thread's pool that are free on its own free list.
    Free(t) == charged[t] - inUse[t] - away[t] - back[t]
    \* The cap the thread's pool has, or gets now if this is its first need: the default.
    Made(t) == IF made[t] # 0 THEN made[t] ELSE Default
end define;

fair process thread \in Pools
begin
T_serve:
    \* `serve_with_cap` as the thread starts serving: its pool, fresh, is held to the runtime's cap.
    \* MUTATION default_cap: nothing holds it, and it keeps the default, as it did before.
    \* MUTATION late_cap: the thread's first job comes before it says which runtime it serves.
    if self \in Others /\ Mutation = "late_cap" then
        skip;                                                                \* (mutated) it serves later
    else
        served[self] := TRUE;                                                \* it serves the runtime
        if ~(self \in Others /\ Mutation = "default_cap") then
            made[self] := Cap;                                               \* `hold_to` the runtime's cap
        end if;
    end if;
T_loop:
    while asked[self] < Takes \/ inUse[self] > 0 \/ away[self] > 0 do
        either                                                               \* the thread's side asks for a buffer:
            \* It has more to ask for, and one to have: a free one, one back, or room under the cap. Else
            \* the side waits for one to come back, and the thread's other sides go on.
            await asked[self] < Takes /\ (Free(self) > 0 \/ back[self] > 0 \/ charged[self] < Made(self));
            made[self] := Made(self);                                        \* the pool made at its first need
            asked[self] := asked[self] + 1;                                  \* one more given
            if Free(self) > 0 then                                           \* a free one: reused
                inUse[self] := inUse[self] + 1;
            elsif back[self] > 0 then                                        \* one came back: taken off the list
                back[self] := back[self] - 1;
                inUse[self] := inUse[self] + 1;
            else                                                             \* room under the cap: a new one
                charged[self] := charged[self] + 1;
                inUse[self] := inUse[self] + 1;
            end if;
        or                                                                   \* (late_cap) the serve after the first job:
            await ~served[self] /\ asked[self] > 0;
            served[self] := TRUE;                                            \* it serves the runtime now
            made[self] := Cap;                                               \* `hold_to`: the cap from now on,
            charged[self] := charged[self] - Min(Free(self), Max(0, charged[self] - Cap)); \* its idle buffers past it freed
        or                                                                   \* a side is done with one, on this thread:
            await inUse[self] > 0;
            inUse[self] := inUse[self] - 1;                                  \* back to the free list, still counted
        or                                                                   \* a handle moves it to another thread:
            await inUse[self] > 0;
            inUse[self] := inUse[self] - 1;
            away[self] := away[self] + 1;                                    \* in use there
        or                                                                   \* dropped on that other thread:
            await away[self] > 0;
            away[self] := away[self] - 1;
            back[self] := back[self] + 1;                                    \* on its own pool's return list
        end either;
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

\* No pool of a thread that serves the runtime holds more than the runtime's cap.
PerPool == \A t \in Pools : served[t] => charged[t] <= Cap

\* All the pools together hold no more than the cap times the pools.
Total == Sum(charged, Pools) <= Cap * Cardinality(Pools)

\* A pool counts every buffer it holds: in use here, away, on the return list, or free.
Counted == \A t \in Pools : inUse[t] + away[t] + back[t] <= charged[t]

\* Every buffer asked for is given in the end: one that waits at the cap gets one back.
AllGiven == <>(\A t \in Pools : asked[t] = Takes)
=============================================================================
