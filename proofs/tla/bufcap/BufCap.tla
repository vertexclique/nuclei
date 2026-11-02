------------------------------ MODULE BufCap ------------------------------
(***************************************************************************)
(* What the IO buffers of a thread that runs no shard may hold             *)
(* (src/buf/mod.rs: `with_pool`, the `FALLBACK` pool made on first need    *)
(* with `fallback_cap`, `serve_with_cap`; `Pool::charge` and `make_room`;  *)
(* src/rt/blocking.rs `Pool::work`, which calls `serve_with_cap` first).   *)
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
(* The pool of such a thread takes the cap of the runtime it serves. A     *)
(* pool thread says which runtime it serves before it runs any job; a      *)
(* thread inside a runtime is inside it before its IO. Before the fix the *)
(* pool took the 64 MiB default whatever the runtime said, so a runtime   *)
(* built to hold 1 MiB of IO buffers per thread held 64 MiB on each of its *)
(* pool threads.                                                           *)
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
    \* The cap the thread's pool has, or gets now if this is its first need.
    \* MUTATION default_cap: the pool takes the default whatever the runtime said, as it did before.
    Made(t) == IF made[t] # 0 THEN made[t]
               ELSE IF Mutation = "default_cap" THEN Default ELSE Cap
end define;

fair process thread \in Pools
begin
T_serve:
    \* MUTATION late_cap: a pool thread's first job needs a buffer before the thread says which runtime it
    \* serves: the pool is made with the default.
    if self \in Others /\ Mutation = "late_cap" then
        made[self] := Default;                                               \* (mutated) made before the cap is known
    end if;
    served[self] := TRUE;                                                    \* `serve_with_cap`, or `enter`, before any IO
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
